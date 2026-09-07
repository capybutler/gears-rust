//! Unit tests for the Plugin Host binding (`Service::get_plugin` /
//! `resolve_plugin`), mirroring the credstore reference
//! (`modules/credstore/credstore/src/domain/service_tests.rs`).
//!
//! Coverage:
//! - resolve + cache (warm call reuses the cached id; same scoped Arc) —
//!   flow `inst-binding-lazy-resolve` / `inst-binding-return-handle`,
//!   algo `inst-algo-binding-get-or-init` / `inst-algo-binding-return`.
//! - registry-unavailable retries on the next call (no error caching) —
//!   algo `inst-algo-binding-catch` / `inst-algo-binding-registry-unavailable`.
//! - `PluginNotFound` on no-match / vendor mismatch —
//!   algo `inst-algo-binding-plugin-not-found`.
//! - `PluginUnavailable` when the scoped slot is empty —
//!   flow `inst-binding-try-get-scoped`, algo `inst-algo-binding-try-get-scoped`.
//! - monotonic binding for the Service lifetime (`reset` exercised in tests only).

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{
    MockTypesRegistryClient, internal as canonical_internal, make_test_instance,
};
use usage_collector_sdk::{UsageCollectorPluginSpecV1, UsageCollectorPluginV1};

use super::*;
use crate::domain::ports::declarations::DeclarationSource;
use crate::domain::test_support::{MockPlugin, UnreachableResolver, enforcer_for};

/// Dummy enforcer for tests that never reach the PDP path
/// (binding / plugin-host tests). An unreachable PDP transport never matters
/// when no authz call is made.
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

// Covers flow `inst-binding-lazy-resolve` / `inst-binding-return-handle` and
// algo `inst-algo-binding-get-or-init` / `inst-algo-binding-resolve-plugin` /
// `inst-algo-binding-return`: the first dispatch resolves single-flight and the
// warm call reuses the cached id (no extra registry round-trip) and the same
// scoped Arc.
#[tokio::test]
async fn get_plugin_resolves_then_caches_resolved_instance() {
    let instance_id = test_instance_id();
    let (hub, registry) =
        hub_with_counting_registry_and_plugin(&instance_id, "cyberfabric", MockPlugin::arc());

    let svc = Service::new(hub, "cyberfabric".into(), dummy_enforcer());
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

// Covers algo `inst-algo-binding-catch` / `inst-algo-binding-registry-unavailable`:
// a failing registry surfaces `TypesRegistryUnavailable`, the selector cache
// stays empty, and the NEXT dispatch retries (proven by list_instance_calls == 2).
#[tokio::test]
async fn get_plugin_retries_resolution_on_each_call_when_registry_fails() {
    let hub = Arc::new(ClientHub::default());
    let registry =
        Arc::new(MockTypesRegistryClient::new().with_list_error(canonical_internal("unavailable")));
    hub.register::<dyn TypesRegistryClient>(registry.clone() as Arc<dyn TypesRegistryClient>);

    let svc = Service::new(hub, "cyberfabric".into(), dummy_enforcer());

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

// Covers algo `inst-algo-binding-registry-unavailable` for the missing-registry
// case: an empty hub (no registered TypesRegistryClient) surfaces
// `TypesRegistryUnavailable` from the explicit hub.get map_err.
#[tokio::test]
async fn get_plugin_returns_registry_unavailable_when_hub_empty() {
    let svc = Service::new(empty_hub(), "cyberfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "expected TypesRegistryUnavailable, got: {err:?}"
    );
}

// ── PluginNotFound ─────────────────────────────────────────────────────────

// Covers algo `inst-algo-binding-plugin-not-found`: no registered instances ->
// `choose_plugin_instance` finds no match -> `PluginNotFound`.
#[tokio::test]
async fn get_plugin_returns_plugin_not_found_when_no_instances() {
    let hub = Arc::new(ClientHub::default());
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(MockTypesRegistryClient::new());
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "cyberfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginNotFound { .. }),
        "expected PluginNotFound, got: {err:?}"
    );
}

// Covers algo `inst-algo-binding-plugin-not-found`: an instance exists but the
// vendor does not match the configured vendor -> `PluginNotFound`.
#[tokio::test]
async fn get_plugin_returns_plugin_not_found_when_vendor_mismatch() {
    let instance_id = test_instance_id();
    let hub = Arc::new(ClientHub::default());
    let instance = make_test_instance(&instance_id, plugin_content(&instance_id, "other-vendor"));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "cyberfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginNotFound { .. }),
        "expected PluginNotFound, got: {err:?}"
    );
}

// Covers algo `inst-algo-binding-resolve-plugin` malformed-content path:
// `choose_plugin_instance` fails to deserialize -> `InvalidPluginInstance`.
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

    let svc = Service::new(hub, "cyberfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::InvalidPluginInstance { .. }),
        "expected InvalidPluginInstance, got: {err:?}"
    );
}

// ── PluginUnavailable (empty scoped slot) ──────────────────────────────────

// Covers flow `inst-binding-try-get-scoped` and algo
// `inst-algo-binding-try-get-scoped`: the registry resolves successfully but the
// scoped client is absent -> `try_get_scoped` returns None -> `PluginUnavailable`.
#[tokio::test]
async fn get_plugin_returns_unavailable_when_scoped_slot_empty() {
    let instance_id = test_instance_id();
    let hub = Arc::new(ClientHub::default());
    let instance = make_test_instance(&instance_id, plugin_content(&instance_id, "cyberfabric"));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "cyberfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginUnavailable { .. }),
        "expected PluginUnavailable, got: {err:?}"
    );
}

// ── monotonic binding for the Service lifetime ─────────────────────────────

// Covers algo `inst-algo-binding-get-or-init` caching semantics: the binding is
// monotonic for the Service lifetime. `GtsPluginSelector::reset` is exercised
// ONLY in unit tests (there is no runtime config-change channel); after reset the
// next dispatch re-resolves, proving the cache is the only re-resolution trigger.
#[tokio::test]
async fn binding_is_monotonic_until_selector_reset() {
    let instance_id = test_instance_id();
    let (hub, registry) =
        hub_with_counting_registry_and_plugin(&instance_id, "cyberfabric", MockPlugin::arc());

    let svc = Service::new(hub, "cyberfabric".into(), dummy_enforcer());

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

// Resolved handle identity is stable across a vendor that selects the lowest
// priority. Verifies the scoped Arc returned by the warm path matches the wired
// mock instance (`hub_with_registry_and_plugin` returns the hub only).
#[tokio::test]
async fn get_plugin_returns_registered_scoped_handle() {
    let instance_id = test_instance_id();
    let hub = hub_with_registry_and_plugin(&instance_id, "cyberfabric", MockPlugin::arc());

    let svc = Service::new(hub, "cyberfabric".into(), dummy_enforcer());
    let resolved = svc.get_plugin().await;
    assert!(
        resolved.is_ok(),
        "expected a resolved scoped handle, got: {:?}",
        resolved.err()
    );
}

// ── injecting a resolver built over a fake declaration source ──────────────
//
// `new_with_metrics` takes a pre-built `Arc<TypeResolver>` rather than a
// `DeclarationSource` or raw cache knobs, so a later task's test wanting a
// fake source just builds a `TypeResolver` over it and passes the resolver
// straight in — no separate wrapper constructor is needed (an earlier
// `Service::new_with_declaration_source` did that job; it became redundant
// once the resolver itself is the injection point, so it was removed rather
// than kept alongside this). Nothing consults the resolver yet, so this only
// proves `new_with_metrics` wires a `Service` whose unrelated behaviour
// (plugin-host binding) is unaffected by what backs the resolver — the fake
// source below panics if it is ever called, which would fail this test
// immediately if that stopped being true.

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
}

#[tokio::test]
async fn new_with_metrics_accepts_a_resolver_built_over_a_fake_source() {
    let instance_id = test_instance_id();
    let hub = hub_with_registry_and_plugin(&instance_id, "cyberfabric", MockPlugin::arc());

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
        "cyberfabric".to_owned(),
        dummy_enforcer(),
        Arc::new(NoopMetrics),
        type_resolver,
        crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
    );

    let resolved = svc.get_plugin().await;
    assert!(
        resolved.is_ok(),
        "expected a resolved scoped handle, got: {:?}",
        resolved.err()
    );
}

// ─── event-deactivation feature ──────────────────────────────────────
//
// `Service::deactivate_usage_record` is the host-side gateway for the
// event-deactivation feature: PDP authz preflight, lazy plugin resolution,
// Plugin SPI Method 5 dispatch, and 1:1 outcome mapping of the plugin
// result taxonomy onto the SDK envelope. Tests pin:
//
// - PDP `deny` short-circuits before the plugin is reached.
// - PDP transport failure (`unreachable`) fails closed before the plugin
//   is reached.
// - The plugin's `Ok(())` propagates to the SDK as `Ok(())`.
// - The plugin's `UsageRecordNotFound { id }` lifts to
//   `UsageCollectorError::NotFound { id }` (carrying the id).
// - The plugin's `UsageRecordAlreadyInactive { id }` lifts to
//   `UsageCollectorError::Conflict { id }` (carrying the id).
mod deactivate_usage_record_tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use toolkit_gts::gts_id;

    use async_trait::async_trait;
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
    use toolkit_security::SecurityContext;
    use types_registry_sdk::TypesRegistryClient;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use usage_collector_sdk::{
        AggregationDimension, AggregationFold, AggregationResult, ConflictReason, MetadataFilter,
        MeterTypeId, TimeRange, USAGE_RECORD_RESOURCE, UsageCollectorError,
        UsageCollectorPluginError, UsageCollectorPluginSpecV1, UsageCollectorPluginV1, UsageRecord,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        CountingTenantPermitResolver, DenyAllResolver, UnreachableResolver, enforcer_for,
    };

    /// Programmable deactivate-stub plugin. Each `deactivate_usage_record`
    /// call drains one response from the queue (FIFO) and records the call
    /// count so tests can pin the exact plugin outcome under test AND verify
    /// the gateway dispatched (or did not dispatch) the SPI capability.
    /// All other SPI methods return a contract-violation — any accidental
    /// dispatch shows up as an obvious test failure, EXCEPT
    /// `get_usage_record` (Method 10), which the deactivation gateway
    /// pre-fetches before the PDP check; tests seed its response through
    /// [`DeactivateStubPlugin::with_get_record`].
    enum DeactivateResponse {
        Ok,
        Err(UsageCollectorPluginError),
    }

    /// Prefetch outcome for [`DeactivateStubPlugin::get_usage_record`]. The
    /// default `Found(_)` lets tests reach the deactivate-SPI call; tests
    /// that drive the `prefetch → NotFound` or `prefetch → plugin error`
    /// branches override via [`DeactivateStubPlugin::with_get_record`].
    enum GetRecordOutcome {
        Found(Box<UsageRecord>),
        NotFound,
        Transient,
    }

    struct DeactivateStubPlugin {
        deactivate_calls: AtomicUsize,
        last_id: Mutex<Option<Uuid>>,
        responses: Mutex<Vec<DeactivateResponse>>,
        get_record_outcome: Mutex<GetRecordOutcome>,
        get_record_calls: AtomicUsize,
    }

    impl DeactivateStubPlugin {
        fn new(responses: Vec<DeactivateResponse>) -> Arc<Self> {
            Arc::new(Self {
                deactivate_calls: AtomicUsize::new(0),
                last_id: Mutex::new(None),
                responses: Mutex::new(responses),
                get_record_outcome: Mutex::new(GetRecordOutcome::Found(Box::new(
                    sample_loaded_record(),
                ))),
                get_record_calls: AtomicUsize::new(0),
            })
        }

        fn with_get_record(self: &Arc<Self>, outcome: GetRecordOutcome) {
            *self.get_record_outcome.lock().expect("mutex not poisoned") = outcome;
        }

        fn deactivate_calls(&self) -> usize {
            self.deactivate_calls.load(Ordering::SeqCst)
        }

        #[allow(dead_code)]
        fn get_record_calls(&self) -> usize {
            self.get_record_calls.load(Ordering::SeqCst)
        }

        fn last_id(&self) -> Option<Uuid> {
            *self.last_id.lock().expect("mutex not poisoned")
        }
    }

    fn sample_loaded_record() -> UsageRecord {
        use time::OffsetDateTime;
        use usage_collector_sdk::{IdempotencyKey, MeterTypeId, ResourceRef, UsageRecordStatus};
        UsageRecord {
            id: Uuid::from_u128(0xAAAA_AAAA),
            gts_type_id: MeterTypeId::new(gts_id!(
                "cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~"
            ))
            .expect("valid usage_record-derived gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new("rsc-stub", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: std::collections::BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new("idem-stub").expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    #[async_trait]
    impl UsageCollectorPluginV1 for DeactivateStubPlugin {
        async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
            self.deactivate_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_id.lock().expect("mutex not poisoned") = Some(id);
            let mut q = self.responses.lock().expect("mutex not poisoned");
            if q.is_empty() {
                return Err(UsageCollectorPluginError::internal(
                    "test_fake: DeactivateStubPlugin: no programmed response remaining",
                ));
            }
            match q.remove(0) {
                DeactivateResponse::Ok => Ok(()),
                DeactivateResponse::Err(err) => Err(err),
            }
        }

        async fn create_usage_record(
            &self,
            _record: UsageRecord,
        ) -> Result<UsageRecord, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: create_usage_record must not be called",
            ))
        }

        async fn create_usage_records(
            &self,
            _records: Vec<UsageRecord>,
        ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
        {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: create_usage_records must not be called",
            ))
        }

        async fn query_aggregated_usage_records(
            &self,
            _gts_type_id: MeterTypeId,
            _time_range: TimeRange,
            _fold: AggregationFold,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _group_by: &[AggregationDimension],
        ) -> Result<AggregationResult, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: query_aggregated_usage_records must not be called",
            ))
        }

        async fn list_usage_records(
            &self,
            _gts_type_id: MeterTypeId,
            _time_range: TimeRange,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
        ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: list_usage_records must not be called",
            ))
        }

        async fn get_usage_record(
            &self,
            id: Uuid,
            _scope: &ast::Expr,
        ) -> Result<UsageRecord, UsageCollectorPluginError> {
            self.get_record_calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self.get_record_outcome.lock().expect("mutex not poisoned");
            match &*outcome {
                GetRecordOutcome::Found(record) => Ok((**record).clone()),
                GetRecordOutcome::NotFound => {
                    Err(UsageCollectorPluginError::UsageRecordNotFound { id })
                }
                GetRecordOutcome::Transient => Err(UsageCollectorPluginError::transient(
                    "test_fake: DeactivateStubPlugin: prefetch transient",
                )),
            }
        }
    }

    fn test_instance_id_for(suffix: &str) -> String {
        format!("{}{suffix}", UsageCollectorPluginSpecV1::gts_type_id())
    }

    fn plugin_content(gts_id: &str, vendor: &str) -> serde_json::Value {
        serde_json::json!({
            "id": gts_id,
            "vendor": vendor,
            "priority": 0,
            "properties": {}
        })
    }

    fn hub_with(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<ClientHub> {
        let hub = Arc::new(ClientHub::default());
        let instance_id = test_instance_id_for(suffix);
        let instance =
            make_test_instance(&instance_id, plugin_content(&instance_id, "cyberfabric"));
        let registry: Arc<dyn TypesRegistryClient> =
            Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
        hub.register::<dyn TypesRegistryClient>(registry);
        hub.register_scoped::<dyn UsageCollectorPluginV1>(
            ClientScope::gts_id(&instance_id),
            plugin,
        );
        hub
    }

    fn authenticated_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(1))
            .subject_tenant_id(Uuid::from_u128(2))
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let enforcer = enforcer_for(CountingTenantPermitResolver::new());
        Arc::new(Service::new(hub, "cyberfabric".into(), enforcer))
    }

    fn service_with_deny(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let enforcer = enforcer_for(Arc::new(DenyAllResolver));
        Arc::new(Service::new(hub, "cyberfabric".into(), enforcer))
    }

    fn service_with_unreachable_pdp(
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let enforcer = enforcer_for(Arc::new(UnreachableResolver));
        Arc::new(Service::new(hub, "cyberfabric".into(), enforcer))
    }

    // ── Happy path: plugin `Ok(())` propagates ────────────────────────

    #[tokio::test]
    async fn deactivate_usage_record_propagates_plugin_ok_to_caller() {
        let plugin = DeactivateStubPlugin::new(vec![DeactivateResponse::Ok]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.ok.v1",
        );
        let id = Uuid::from_u128(0x00C0_FFEE);

        svc.deactivate_usage_record(&authenticated_ctx(), id)
            .await
            .expect("plugin Ok(()) MUST propagate as SDK Ok(())");

        assert_eq!(
            plugin.deactivate_calls(),
            1,
            "the SPI capability MUST be invoked exactly once on the success path",
        );
        assert_eq!(
            plugin.last_id(),
            Some(id),
            "the gateway MUST forward the target id verbatim to the plugin",
        );
    }

    // ── PDP deny collapses to NotFound BEFORE plugin dispatch ─────────

    #[tokio::test]
    async fn deactivate_usage_record_pdp_deny_collapses_to_not_found_before_plugin() {
        let plugin = DeactivateStubPlugin::new(vec![]);
        let svc = service_with_deny(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.deny.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0xFEED))
            .await
            .expect_err("PDP deny MUST surface as NotFound");

        // Denial collapses to NotFound (existence-oracle guard), never PermissionDenied.
        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
        assert_eq!(
            plugin.deactivate_calls(),
            0,
            "PDP deny MUST short-circuit BEFORE any Plugin SPI dispatch",
        );
    }

    // ── PDP transport failure fails closed BEFORE plugin dispatch ─────

    #[tokio::test]
    async fn deactivate_usage_record_pdp_unreachable_fails_closed_before_plugin() {
        let plugin = DeactivateStubPlugin::new(vec![]);
        let svc = service_with_unreachable_pdp(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.unreachable.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0xFACE))
            .await
            .expect_err("unreachable PDP transport MUST fail closed");

        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable (PDP transport failure), got {err:?}",
        );
        assert_eq!(
            plugin.deactivate_calls(),
            0,
            "PDP transport failure MUST fail closed BEFORE any Plugin SPI dispatch",
        );
    }

    // ── Plugin `UsageRecordNotFound { id }` → SDK `NotFound(id)` ───────

    #[tokio::test]
    async fn deactivate_usage_record_plugin_not_found_lifts_to_sdk_not_found() {
        let id = Uuid::from_u128(0xDEAD_BEEF);
        let plugin = DeactivateStubPlugin::new(vec![DeactivateResponse::Err(
            UsageCollectorPluginError::UsageRecordNotFound { id },
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.notfound.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), id)
            .await
            .expect_err("unknown id MUST surface as NotFound");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, .. }
                    if resource_type == USAGE_RECORD_RESOURCE && name == &id.to_string()
            ),
            "expected NotFound carrying the target id, got {err:?}",
        );
        assert_eq!(plugin.deactivate_calls(), 1);
    }

    // ── Plugin `UsageRecordAlreadyInactive { id }` →
    //     SDK `AlreadyInactive { id }` ──────────────────────────────────

    #[tokio::test]
    async fn deactivate_usage_record_plugin_already_inactive_lifts_to_sdk_already_inactive() {
        // `cpt-cf-usage-collector-dod-event-deactivation-entity-deactivation-status`:
        // a second deactivation against an already-inactive record MUST surface
        // the actionable `AlreadyInactive` SDK variant. The canonical lift then
        // emits HTTP 409 with `context.reason="ALREADY_INACTIVE"`.
        let id = Uuid::from_u128(0xCAFE_BABE);
        let plugin = DeactivateStubPlugin::new(vec![DeactivateResponse::Err(
            UsageCollectorPluginError::UsageRecordAlreadyInactive { id },
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.already_inactive.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), id)
            .await
            .expect_err("already-inactive target MUST surface as AlreadyInactive");

        assert!(
            matches!(
                err,
                UsageCollectorError::Conflict {
                    reason: ConflictReason::AlreadyInactive,
                    ref name,
                    ..
                } if name == &id.to_string()
            ),
            "expected Conflict(AlreadyInactive) carrying the target id, got {err:?}",
        );
        assert_eq!(plugin.deactivate_calls(), 1);
    }

    // ── Plugin transport / readiness fault → 503 envelope ─────────────

    #[tokio::test]
    async fn deactivate_usage_record_plugin_transient_lifts_to_service_unavailable_envelope() {
        let plugin = DeactivateStubPlugin::new(vec![DeactivateResponse::Err(
            UsageCollectorPluginError::transient("downstream connection reset"),
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.transient.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0x01))
            .await
            .expect_err("plugin Transient MUST lift to ServiceUnavailable");

        match &err {
            UsageCollectorError::ServiceUnavailable { detail, .. } => {
                assert_eq!(detail, "downstream connection reset");
            }
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
        assert!(err.is_retryable());
    }

    // ── Prefetch returns Err(UsageRecordNotFound) → NotFound, never reaches
    //     PDP or Method 5 dispatch (the host-side pre-PDP existence check) ──

    #[tokio::test]
    async fn deactivate_usage_record_prefetch_not_found_skips_pdp_and_spi() {
        // `cpt-cf-usage-collector-flow-event-deactivation-deactivate-record`
        // step `inst-deactivate-record-prefetch-not-found`: a missing target
        // surfaces as `NotFound(id)` BEFORE the PDP authz call AND BEFORE
        // the SPI Method 5 dispatch. The PDP is deny-all to prove the
        // prefetch-not-found path short-circuits past it.
        let id = Uuid::from_u128(0xDEAD_F00D);
        let plugin = DeactivateStubPlugin::new(vec![]);
        plugin.with_get_record(GetRecordOutcome::NotFound);
        let svc = service_with_deny(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.prefetch_not_found.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), id)
            .await
            .expect_err("prefetch UsageRecordNotFound MUST surface as NotFound");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, .. }
                    if resource_type == USAGE_RECORD_RESOURCE && name == &id.to_string()
            ),
            "expected NotFound carrying the target id, got {err:?}",
        );
        assert_eq!(
            plugin.deactivate_calls(),
            0,
            "prefetch-not-found MUST short-circuit BEFORE any Method 5 dispatch",
        );
    }

    // ── Prefetch surfaces a plugin transport fault → propagates via the
    //     From-impl chain (host does NOT swallow the variant) ────────────

    #[tokio::test]
    async fn deactivate_usage_record_prefetch_transient_propagates() {
        // A storage fault during prefetch propagates verbatim through the
        // From-impl chain; the deactivate handler MUST NOT silently retry
        // and MUST NOT dispatch the deactivate SPI call.
        let plugin = DeactivateStubPlugin::new(vec![]);
        plugin.with_get_record(GetRecordOutcome::Transient);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.prefetch_transient.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0xBEEF))
            .await
            .expect_err("prefetch Transient MUST lift to ServiceUnavailable");

        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable from prefetch, got {err:?}",
        );
        assert_eq!(
            plugin.deactivate_calls(),
            0,
            "prefetch fault MUST short-circuit BEFORE deactivate SPI dispatch",
        );
    }
}

// ── PDP dedup pre-pass in `create_usage_records` ───────────────────────────
//
// Pins the intra-batch dedup behavior described in
// `cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization`
// instructions `inst-algo-attrib-dedup-tuple-key` and
// `inst-algo-attrib-bounded-fanout`: records sharing the same attribution
// tuple `(tenant_id, resource_type, resource_id, subject_id, subject_type)`
// MUST collapse to a single PDP `evaluate` round-trip, projected onto every
// input index in the group.
#[cfg(test)]
mod pdp_dedup_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef, SubjectRef,
        UsageCollectorPluginV1, UsageRecord, UsageRecordStatus,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
    };

    const HAPPY_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] wired with an arbitrary working declaration
    /// (these tests exercise PDP dedup, not the declaration itself),
    /// exposing the [`CountingTenantPermitResolver`] handle.
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
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    fn input_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> CreateUsageRecord {
        // Distinct `idem` values keep records distinct even when they share an
        // attribution tuple; the create surface is identity-free (the id is
        // derived from the dedup identity inside the service).
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(HAPPY_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
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
    /// `evaluate` round-trip. This pins
    /// `inst-algo-attrib-dedup-tuple-key`.
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

    /// Three records, three distinct `resource_id` values → exactly three
    /// PDP calls (each tuple key is unique, no dedup possible). Pins the
    /// "one call per distinct tuple" contract from `inst-algo-attrib-bounded-fanout`.
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
                        r.idempotency_key.as_str(),
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
                        r.idempotency_key.as_str(),
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

    /// Deny-side projection. When PDP DENIES one tuple group, every input
    /// index in that group MUST surface as a rejected record; every input
    /// index in a permitted tuple group MUST proceed. Pins the deny half
    /// of `inst-algo-attrib-dedup-tuple-key`, the sibling of
    /// `create_usage_records_projects_pdp_decision_across_shared_tuple_groups`
    /// for permits.
    #[tokio::test]
    async fn create_usage_records_projects_pdp_deny_across_shared_tuple_groups() {
        use crate::domain::service::Service;
        use crate::domain::test_support::{DenyOneResourceResolver, enforcer_for, hub_with_plugin};

        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xEE);
        // Two tuple groups: rsc-DENY (denied) and rsc-OK (permitted), two
        // records per group. The deny MUST project across both rsc-DENY
        // indices; the permit MUST project across both rsc-OK indices.
        let input = vec![
            input_record(tenant_id, "rsc-DENY", "idem-D-0"),
            input_record(tenant_id, "rsc-OK", "idem-K-0"),
            input_record(tenant_id, "rsc-DENY", "idem-D-1"),
            input_record(tenant_id, "rsc-OK", "idem-K-1"),
        ];

        // Only the two permitted records reach the plugin SPI; programme
        // exactly those persisted responses.
        plugin.set_create_records(
            input
                .iter()
                .filter(|r| r.resource_ref.resource_id() == "rsc-OK")
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let hub = hub_with_plugin(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.deny_projection.records.v1",
            "cyberfabric",
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
            "cyberfabric".to_owned(),
            enforcer,
            Arc::new(crate::domain::ports::metrics::NoopMetrics),
            type_resolver,
            crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
        ));

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded - per-record outcomes carry deny");

        assert_eq!(results.len(), 4);
        // Indices 0 and 2 carry rsc-DENY; both MUST surface
        // `UsageCollectorError::PermissionDenied` from the projected deny.
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
        // Indices 1 and 3 carry rsc-OK; both MUST be accepted.
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
                        r.idempotency_key.as_str(),
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
// Pins the intra-batch declaration-resolution dedup behavior described in
// `cpt-cf-usage-collector-algo-usage-emission-catalog-existence-and-kind-lookup`
// instructions `inst-algo-catalog-dedup-gts-id` and
// `inst-algo-catalog-bounded-fanout`: records sharing the same meter
// `gts_type_id` MUST collapse to a single Type Resolver round-trip,
// projected onto every input index referencing that id.
#[cfg(test)]
mod gts_type_id_dedup_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef, USAGE_RECORD_RESOURCE,
        UsageCollectorError, UsageCollectorPluginV1, UsageRecord, UsageRecordStatus,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_counting,
        fake_declaration_source_with_one_unresolvable,
    };

    const GTS_A: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
    const GTS_B: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_emitted.v1~");
    const GTS_C: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_buffered.v1~");

    fn meter_id_for(gts: &str) -> MeterTypeId {
        MeterTypeId::new(gts).expect("valid meter type id")
    }

    fn record_for(gts: &str, tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: meter_id_for(gts),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-gts-dedup", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
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
            value: input.value,
            idempotency_key: input.idempotency_key.clone(),
            corrects_id: input.corrects_id,
            status: UsageRecordStatus::Active,
            window_start: input.window_start,
            window_end: input.window_end,
        }
    }

    // The "5 records, identical gts_type_id → exactly one resolution" case is
    // now covered by `ingestion_declared_type_tests::a_batch_resolves_each_distinct_type_once`
    // (Task 9's own required test, using `CountingDeclarationSource`) —
    // deleted here rather than duplicated.

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

        // Only the two resolvable-gts_type_id records reach the SPI; program
        // two accepted-persist responses.
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

// ── corrects_id L1 dedup pre-pass in `create_usage_records` ────────────────
//
// Pins the intra-batch L1-lookup dedup behavior described in
// `cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2`
// instructions `inst-algo-semantics-l1-dedup` and
// `inst-algo-semantics-l1-bounded-fanout`: records sharing the same
// `corrects_id` MUST collapse to a single `get_usage_record` SPI
// round-trip, and ordinary (non-compensation) records MUST NOT trigger
// any L1 lookup at all.
#[cfg(test)]
mod corrects_id_dedup_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef, USAGE_RECORD_RESOURCE,
        UsageCollectorError, UsageCollectorPluginV1, UsageRecord, UsageRecordStatus,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] wired with an arbitrary working declaration —
    /// these tests exercise the L1 `corrects_id` dedup pre-pass, not the
    /// declaration itself.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    fn referenced_original(tenant_id: Uuid) -> UsageRecord {
        // The L1 verifier checks (corrects_id IS NULL, identity-tuple match,
        // status=Active) against this row, so the compensation records under
        // test must mirror its (tenant, gts_type_id, resource_ref, subject_ref)
        // shape. `set_get_record` returns this same row for any id the
        // host looks up — that's fine because verify_l1_corrects_id reads
        // identity fields, not id.
        UsageRecord {
            id: Uuid::from_u128(0xDEAD_BEEF),
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-comp", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(10),
            idempotency_key: IdempotencyKey::new("idem-original").expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    fn compensation_for(tenant_id: Uuid, corrects_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-comp", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(-1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: Some(corrects_id),
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    fn ordinary_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-comp", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    /// Five compensations sharing one `corrects_id` MUST collapse to a
    /// single `get_usage_record` SPI round-trip.
    #[tokio::test]
    async fn create_usage_records_collapses_get_usage_record_calls_for_shared_corrects_id() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x501);
        plugin.set_get_record(referenced_original(tenant_id));

        let corrects_id = Uuid::from_u128(0x601);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| compensation_for(tenant_id, corrects_id, &format!("idem-comp-{i}")))
            .collect();

        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.l1_dedup.shared.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(
            results.iter().all(Result::is_ok),
            "every compensation MUST be accepted: {results:?}",
        );

        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "5 compensations sharing one corrects_id MUST collapse to a single \
             get_usage_record SPI dispatch (intra-batch L1 dedup); observed {} calls",
            plugin.get_usage_record_calls(),
        );
    }

    /// Three distinct `corrects_id`s MUST produce three `get_usage_record`
    /// dispatches.
    #[tokio::test]
    async fn create_usage_records_issues_one_get_usage_record_call_per_distinct_corrects_id() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x502);
        plugin.set_get_record(referenced_original(tenant_id));

        let corrects_id_a = Uuid::from_u128(0x602);
        let corrects_id_b = Uuid::from_u128(0x603);
        let corrects_id_c = Uuid::from_u128(0x604);

        let input = vec![
            compensation_for(tenant_id, corrects_id_a, "idem-A"),
            compensation_for(tenant_id, corrects_id_b, "idem-B"),
            compensation_for(tenant_id, corrects_id_c, "idem-C"),
        ];

        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.l1_dedup.distinct.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            plugin.get_usage_record_calls(),
            3,
            "3 distinct corrects_ids MUST produce 3 get_usage_record SPI dispatches; \
             observed {} calls",
            plugin.get_usage_record_calls(),
        );

        let mut seen = plugin.get_usage_record_inputs();
        seen.sort();
        let mut expected = vec![corrects_id_a, corrects_id_b, corrects_id_c];
        expected.sort();
        assert_eq!(
            seen, expected,
            "the deduped fan-out MUST ask the plugin for exactly the distinct corrects_ids",
        );
    }

    /// Ordinary records (no `corrects_id`) MUST NOT trigger any L1 lookup.
    #[tokio::test]
    async fn create_usage_records_skips_l1_lookup_when_no_record_has_corrects_id() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x503);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| ordinary_record(tenant_id, &format!("idem-ord-{i}")))
            .collect();

        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.l1_dedup.no_corrects.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "ordinary records (corrects_id IS NULL) MUST NOT trigger L1 lookups; \
             observed {} get_usage_record dispatches",
            plugin.get_usage_record_calls(),
        );
    }

    /// L1 not-found for one `corrects_id` MUST project to every record
    /// sharing it (rejected as `CorrectsIdNotFound`), while records
    /// referencing a different known `corrects_id` are still accepted.
    #[tokio::test]
    async fn create_usage_records_projects_l1_not_found_to_every_record_sharing_unknown_corrects_id()
     {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x504);
        plugin.set_get_record(referenced_original(tenant_id));

        let corrects_id_good = Uuid::from_u128(0x605);
        let corrects_id_bad = Uuid::from_u128(0x606);
        plugin.set_get_usage_record_not_found(corrects_id_bad);

        let input = vec![
            compensation_for(tenant_id, corrects_id_bad, "idem-bad-0"),
            compensation_for(tenant_id, corrects_id_good, "idem-good-0"),
            compensation_for(tenant_id, corrects_id_bad, "idem-bad-1"),
        ];

        // Only the known-good record reaches the persist SPI; program one
        // accepted response.
        plugin.set_create_records(vec![Ok(projected(&input[1]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.l1_dedup.mixed_not_found.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);

        assert!(
            results[1].is_ok(),
            "record at index 1 (known corrects_id) MUST be accepted, got {:?}",
            results[1],
        );
        for idx in [0usize, 2] {
            match results[idx].as_ref() {
                Err(UsageCollectorError::NotFound {
                    resource_type,
                    detail,
                    ..
                }) if resource_type == USAGE_RECORD_RESOURCE && detail.contains("corrects_id") => {
                    assert!(
                        detail.contains(&corrects_id_bad.to_string()),
                        "record at index {idx} MUST surface CorrectsIdNotFound carrying \
                         the unknown corrects_id",
                    );
                }
                other => panic!(
                    "record at index {idx} MUST be rejected as CorrectsIdNotFound, got {other:?}",
                ),
            }
        }

        assert_eq!(
            plugin.get_usage_record_calls(),
            2,
            "2 distinct corrects_ids carrying 3 records MUST produce 2 get_usage_record \
             dispatches; observed {} calls",
            plugin.get_usage_record_calls(),
        );
    }
}

// ─── usage-emission feature (read-by-id) ─────────────────────────────────
//
// `Service::get_usage_record` is the host-side gateway for the read-by-id
// surface of the usage-emission feature. Per DESIGN §3.3 / §3.5 (Task 13)
// it now authorizes like `list_usage_records` / `query_aggregated_usage_records`
// rather than like `deactivate_usage_record`: a pre-row PDP call
// (`authz::authorize_get_usage_record_scope`, no per-record attribution
// attributes — the id-only boundary doesn't have the record's tenant /
// resource / subject fields to offer yet) compiles the caller's scope
// FIRST, and only a permitted caller's compiled scope ever reaches Plugin
// SPI Method 10 `get_usage_record(id, scope)`. The point lookup carries no
// caller filter, so the compiled scope is the whole plugin-side filter — a
// row outside it is never returned, mirroring "does not exist". Tests pin:
//
// - Happy path: PDP permits, the plugin returns the row, it's returned
//   verbatim. Exactly one `get_usage_record` SPI dispatch, carrying a
//   compiled scope (not merely succeeding — see
//   `the_point_lookup_passes_the_compiled_scope_to_the_plugin`).
// - The plugin reports `UsageRecordNotFound { id }` for a permitted
//   caller's genuinely-missing target → lifted to
//   `UsageCollectorError::NotFound { id }`.
// - PDP `deny` collapses to that same `NotFound` — BEFORE any plugin
//   dispatch, since authorization now runs first: the plugin never even
//   sees the id, let alone the row.
// - PDP transport failure (`unreachable`) fails closed, also before any
//   plugin dispatch.
// - A plugin-side transient error (on an otherwise-permitted call) lifts
//   to `ServiceUnavailable`.
mod get_usage_record_tests {
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginV1, UsageRecord,
        UsageRecordStatus,
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
        use usage_collector_sdk::{IdempotencyKey, MeterTypeId, ResourceRef};
        UsageRecord {
            id,
            gts_type_id: MeterTypeId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-happy", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new("idem-happy").expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    /// Happy path: PDP permits (a real, tenant-narrowing compiled scope —
    /// see [`recording_plugin_resolver`]), the plugin returns the row, the
    /// service returns it verbatim. Exactly one `get_usage_record` SPI
    /// dispatch.
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
    }

    /// A permitted caller's genuinely-missing target (the plugin reports
    /// `UsageRecordNotFound { id }`) surfaces as `NotFound` — the ordinary
    /// missing-row case, distinct from the PDP-deny case below even though
    /// both converge on the same envelope (that convergence is the whole
    /// point: see the module doc comment).
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

    /// PDP deny collapses to `NotFound` — BEFORE any plugin dispatch.
    /// Authorization now runs first (a pre-row compiled-scope request, like
    /// LIST/aggregate), so a denied caller's scope never reaches the
    /// plugin at all: the row is never even queried, let alone returned.
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

    /// PDP transport failure fails closed — also before any plugin
    /// dispatch, for the same reason as the deny case above.
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

    /// The plugin receives the compiled PDP scope on the point lookup —
    /// DESIGN §3.3 / Task 13's central guarantee. Asserts the spy actually
    /// captured a scope expression (not merely that the call completed);
    /// the captured `Debug` rendering must carry the tenant-narrowing
    /// predicate `recording_plugin_resolver` grants, so a regression that
    /// wires an unrestricted/placeholder filter through instead (rather
    /// than the real compiled scope) is caught, not just "some string".
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
    /// lifts through the canonical chain to `ServiceUnavailable` — same
    /// envelope the deactivate gateway emits on its own prefetch transient.
    #[tokio::test]
    async fn get_usage_record_plugin_transient_lifts_to_service_unavailable() {
        use usage_collector_sdk::UsageCollectorPluginError;

        // Build a tiny one-shot stub that always returns Transient on
        // get_usage_record; HappyPathPlugin doesn't expose a transient
        // path so we use the existing pattern with the deactivate-stub
        // approach folded down to a minimal inline plugin.
        struct TransientGetPlugin;

        #[async_trait::async_trait]
        impl UsageCollectorPluginV1 for TransientGetPlugin {
            async fn create_usage_record(
                &self,
                _record: UsageRecord,
            ) -> Result<UsageRecord, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: create_usage_record must not be called",
                ))
            }
            async fn create_usage_records(
                &self,
                _records: Vec<UsageRecord>,
            ) -> Result<
                Vec<Result<UsageRecord, UsageCollectorPluginError>>,
                UsageCollectorPluginError,
            > {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: create_usage_records must not be called",
                ))
            }
            async fn query_aggregated_usage_records(
                &self,
                _gts_type_id: usage_collector_sdk::MeterTypeId,
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
                _gts_type_id: usage_collector_sdk::MeterTypeId,
                _time_range: usage_collector_sdk::TimeRange,
                _query: &toolkit_odata::ODataQuery,
                _metadata_filter: &[usage_collector_sdk::MetadataFilter],
            ) -> Result<toolkit_odata::Page<UsageRecord>, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: list_usage_records must not be called",
                ))
            }
            async fn deactivate_usage_record(
                &self,
                _id: Uuid,
            ) -> Result<(), UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: deactivate_usage_record must not be called",
                ))
            }
            async fn get_usage_record(
                &self,
                _id: Uuid,
                _scope: &toolkit_odata::ast::Expr,
            ) -> Result<UsageRecord, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::transient(
                    "test_fake: TransientGetPlugin: simulated prefetch transient",
                ))
            }
        }

        let plugin: Arc<dyn UsageCollectorPluginV1> = Arc::new(TransientGetPlugin);
        // The PDP MUST permit here (`recording_plugin_resolver`, not the
        // fixture's tenant-echoing default — which would fail closed on
        // this pre-row request and mask the plugin's Transient behind an
        // authz NotFound) so the flow actually reaches the plugin dispatch
        // this test means to exercise.
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
    //! Unit coverage for the private service-module helpers
    //! (`compose_query_with_scope`). Lives alongside the SDK-surface tests
    //! in this file so the codebase keeps one test module per source.

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
            Box::new(Expr::Identifier("status".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("active".into()))),
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

// The plural `create_usage_records` path is exercised by the `pdp_dedup`,
// `gts_id_dedup`, and `corrects_id_dedup` modules; the singular path has
// its own PDP / catalog / semantics / L1 / SPI sequencing in
// `service.rs::create_usage_record` and was uncovered. These tests pin one
// outcome per stage: PDP deny, plugin-reported transient on the persist
// SPI, semantics violations (negative counter + gauge compensation),
// L1 corrects_id not-found, L1 corrects_id wrong-scope, and the happy
// path. The mirror keeps a single source-of-truth for what "every stage
// rejects with its locked SDK envelope" means for the singular flow.

mod create_usage_record_path_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        ConflictReason, CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef,
        USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginError,
        UsageCollectorPluginV1, UsageRecord, UsageRecordStatus,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        DenyAllResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx, enforcer_for,
        fake_declaration_source_with_fold, hub_with_plugin, projected,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// Build an ordinary record (no `corrects_id`) with the given
    /// `tenant_id` and `value`. Used as the base shape every test in this
    /// module shapes — call sites mutate `value` / `gts_type_id` /
    /// `corrects_id` to drive the per-stage outcome. There is no more
    /// caller-visible `kind` to gate a value-sign rule on, so `value` no longer
    /// drives any accept/reject decision here — it is carried only because
    /// `CreateUsageRecord` requires one.
    fn counter_record(tenant_id: Uuid, value: i64, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-singular", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(value),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    fn counter_compensation(tenant_id: Uuid, corrects_id: Uuid, idem: &str) -> CreateUsageRecord {
        let mut r = counter_record(tenant_id, -1, idem);
        r.corrects_id = Some(corrects_id);
        r
    }

    /// Build a `Service` wired against a permit-by-default PDP, a working
    /// Type Resolver declaring an arbitrary fold (these tests exercise
    /// ingestion, not the aggregate path — the fold value is irrelevant),
    /// and the supplied plugin stub.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// Build a `Service` wired against a deny-all PDP. The deny path
    /// MUST short-circuit before any plugin SPI dispatch; the plugin
    /// stub is left unprogrammed so a leaked SPI call surfaces as a
    /// `not_programmed` `Internal` (distinct from the expected
    /// `Authorization` envelope) and fails the test loudly.
    fn service_with_deny(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with_plugin(plugin, suffix, "cyberfabric");
        let enforcer = enforcer_for(Arc::new(DenyAllResolver) as _);
        Arc::new(Service::new(hub, "cyberfabric".to_owned(), enforcer))
    }

    /// PDP deny ⇒ `Authorization` envelope, **no** catalog / semantics /
    /// SPI dispatch. The unprogrammed plugin asserts the short-circuit:
    /// any leaked SPI call would surface as `Internal("not programmed")`
    /// instead of `Authorization` and the assertion below would fail.
    #[tokio::test]
    async fn create_usage_record_pdp_deny_returns_authorization_before_any_spi_dispatch() {
        let plugin = HappyPathPlugin::new();
        let service = service_with_deny(
            Arc::clone(&plugin) as _,
            "test.singular.pdp_deny.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x701), 1, "idem-pdp-deny");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("PDP deny MUST surface as Err");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "PDP deny MUST surface as `PermissionDenied`, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "PDP deny MUST short-circuit before any L1 lookup",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "PDP deny MUST short-circuit before any persist SPI dispatch",
        );
    }

    /// Plugin-reported `Transient` from the persist SPI ⇒
    /// `ServiceUnavailable` at the SDK boundary, with the
    /// `retry_after_seconds` hint forwarded verbatim. Pins the lift
    /// path documented on `UsageCollectorPluginError::transient_with_retry`.
    #[tokio::test]
    async fn create_usage_record_plugin_transient_lifts_to_service_unavailable() {
        let plugin = HappyPathPlugin::new();
        plugin.set_create_record_err(UsageCollectorPluginError::transient_with_retry(
            "downstream backend timed out",
            Some(13),
        ));

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.plugin_transient.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x702), 1, "idem-plugin-transient");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("plugin transient MUST surface as Err");

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

    /// `corrects_id` references a uuid the plugin does not have ⇒
    /// `CorrectsIdNotFound`. Pins the L1 referential rule 1 lift on the
    /// singular path (the plural path has its own coverage in
    /// `corrects_id_dedup_tests`).
    #[tokio::test]
    async fn create_usage_record_l1_corrects_id_not_found_returns_typed_error() {
        let plugin = HappyPathPlugin::new();

        let missing = Uuid::from_u128(0x801);
        plugin.set_get_usage_record_not_found(missing);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.l1_not_found.records.v1",
        );

        let record = counter_compensation(Uuid::from_u128(0x705), missing, "idem-l1-missing");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("unknown corrects_id MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref detail, .. }
                    if resource_type == USAGE_RECORD_RESOURCE
                        && detail.contains("corrects_id")
                        && detail.contains(&missing.to_string())
            ),
            "L1 referential rule 1 MUST surface as `CorrectsIdNotFound` \
             carrying the caller-supplied corrects_id; got {err:?}",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "L1 not-found MUST short-circuit before the persist SPI",
        );
    }

    /// `corrects_id` references a row in a different `tenant_id` ⇒
    /// `CorrectsIdWrongScope`. Pins the L1 referential rule 3 lift on
    /// the singular path: the verifier reads identity-tuple fields
    /// (tenant, `gts_type_id`, `resource_ref`, `subject_ref`) off the
    /// referenced row and rejects on the first mismatch.
    #[tokio::test]
    async fn create_usage_record_l1_corrects_id_wrong_scope_returns_typed_error() {
        let plugin = HappyPathPlugin::new();

        let referenced_tenant = Uuid::from_u128(0x901);
        let other_tenant = Uuid::from_u128(0x902);
        let corrects_id = Uuid::from_u128(0x802);

        // Referenced row sits in `referenced_tenant`; the incoming
        // compensation will be shaped under `other_tenant` so the
        // identity-tuple comparison fails on the tenant axis.
        let referenced = UsageRecord {
            id: corrects_id,
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id: referenced_tenant,
            resource_ref: ResourceRef::new("rsc-singular", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(10),
            idempotency_key: IdempotencyKey::new("idem-referenced").expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        };
        plugin.set_get_record(referenced);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.l1_wrong_scope.records.v1",
        );

        let record = counter_compensation(other_tenant, corrects_id, "idem-l1-wrong-scope");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("cross-tenant corrects_id MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::Conflict {
                    reason: ConflictReason::CorrectsIdWrongScope,
                    ref name,
                    ref detail,
                    ..
                } if name == &corrects_id.to_string()
                    || detail.contains(&corrects_id.to_string()),
            ),
            "L1 referential rule 3 MUST surface as `CorrectsIdWrongScope` \
             carrying the caller-supplied corrects_id; got {err:?}",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "L1 wrong-scope MUST short-circuit before the persist SPI",
        );
    }

    /// Happy path: PDP permit + catalog hit + ordinary counter semantics +
    /// metadata validation pass + persist SPI returns the persisted echo
    /// ⇒ `Ok(persisted_record)`. The persisted echo's `id` differs
    /// from the input so the returned record can be distinguished from
    /// the caller-supplied one.
    #[tokio::test]
    async fn create_usage_record_happy_path_returns_persisted_echo() {
        let plugin = HappyPathPlugin::new();

        let mut persisted = projected(&counter_record(
            Uuid::from_u128(0xCAFE),
            1,
            "idem-happy-persist",
        ));
        // Distinguish persisted from input — the plugin's persisted echo
        // carries a different id than the record the service derives and
        // dispatches.
        persisted.id = Uuid::from_u128(0xDEAD_C0DE);
        plugin.set_create_record(persisted.clone());

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.happy_path.records.v1",
        );

        let input = counter_record(Uuid::from_u128(0x706), 1, "idem-happy-input");

        let returned = service
            .create_usage_record(&authenticated_ctx(), input)
            .await
            .expect("happy path MUST return the persisted record");

        assert_eq!(
            returned.id, persisted.id,
            "the returned record MUST be the plugin's persisted echo \
             (not the caller's input)",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "ordinary counter records (corrects_id IS NULL) MUST NOT \
             trigger an L1 lookup",
        );
    }
}

// ── Batch size-cap guard in `create_usage_records` ─────────────────────────
//
// Pins the entry gate at `service.rs::create_usage_records` (the
// `actual == 0 || actual > MAX_BATCH_RECORDS` check,
// `inst-emit-batch-cap-check`): both out-of-bounds arms MUST reject with
// `invalid_batch_size` *before* any plugin dispatch.
#[cfg(test)]
mod batch_size_cap_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef, UsageCollectorError,
        UsageCollectorPluginV1, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::service::MAX_BATCH_RECORDS;
    use crate::domain::test_support::{HappyPathPlugin, ServiceFixture, authenticated_ctx};

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn input_record(idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc-batch-cap", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
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

        let service = ServiceFixture::default().build(
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

    /// Over-cap batch (`MAX_BATCH_RECORDS + 1`) → `invalid_batch_size`,
    /// plugin untouched.
    #[tokio::test]
    async fn create_usage_records_rejects_over_cap_batch_without_dispatch() {
        let plugin = HappyPathPlugin::new();

        let service = ServiceFixture::default().build(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.batch_cap.over.records.v1",
        );

        let input: Vec<CreateUsageRecord> = (0..=MAX_BATCH_RECORDS)
            .map(|i| input_record(&format!("idem-cap-{i}")))
            .collect();
        assert_eq!(input.len(), MAX_BATCH_RECORDS + 1);

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
}

// ── Service-level derived-id stamp (in-process / SDK callers) ───────────────
//
// The create surface is identity-free (`CreateUsageRecord`): callers never
// supply an `id`. The domain `Service` is the single, guaranteed point where a
// submission acquires its identity — via
// `CreateUsageRecord::try_into_usage_record`, which validates the covered
// period and then derives the `id` from the 5-tuple dedup identity. These
// tests drive the `Service` create methods directly (NOT through the REST
// handler) and assert the record the plugin RECEIVED carries the
// deterministic derivation, pinning that the service stamps the derived id on
// the dispatch path.
#[cfg(test)]
mod derived_id_stamp_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef, UsageCollectorPluginV1,
        derive_usage_record_id,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] wired with an arbitrary working declaration —
    /// these tests exercise id derivation, not the declaration itself.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// Build a create submission with a known dedup identity
    /// (`tenant_id` / `gts_type_id` / `idempotency_key` / `window_start` /
    /// `window_end`), so a passing assertion can only mean the service
    /// derived the dispatched record's id from it.
    fn input_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-derive", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    /// Singular `create_usage_record`: the dispatched record's `id` MUST be the
    /// service-derived value.
    #[tokio::test]
    async fn create_usage_record_stamps_derived_id() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD1);
        let idem = "idem-derive-singular";
        let input = input_record(tenant_id, idem);
        let expected = derive_usage_record_id(
            input.tenant_id,
            &input.gts_type_id,
            &input.idempotency_key,
            input.window_start,
            input.window_end,
        );

        // The plugin echoes back the record it was dispatched, so the persist
        // SPI succeeds; the assertion reads the CAPTURED dispatched record.
        plugin.set_create_record(projected(&input));

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.derived_id.singular.records.v1",
        );

        service
            .create_usage_record(&authenticated_ctx(), input)
            .await
            .expect("happy path MUST accept the record");

        let dispatched = plugin
            .last_create_record_input()
            .expect("plugin received the dispatched record");
        assert_eq!(
            dispatched.id, expected,
            "the SERVICE MUST stamp the dispatched record's id with \
             derive_usage_record_id(tenant_id, gts_type_id, idempotency_key, \
             window_start, window_end) - this guards the in-process (non-REST) \
             caller path independently of the handler",
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
                    &r.idempotency_key,
                    r.window_start,
                    r.window_end,
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
}

// ── Covered-period preconditions on the batch path ─────────────────────────
//
// The identity derivation is per-submission and fallible, so a rejected
// covered period is a PER-RECORD outcome routed to its own input index —
// never a batch-level failure, and never a slot shifted onto a neighbour.
// The batch path's surviving vector is no longer index-aligned with the
// input, which is exactly the mistake these tests exist to catch.
#[cfg(test)]
mod covered_period_batch_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef, UsageCollectorError,
        UsageCollectorPluginV1,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        DenyOneResourceResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_fold, projected,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// Tenant `2` clears the fixture PDP; distinct `idem` values keep the
    /// submissions distinct without changing their attribution tuple, so the
    /// batch takes ONE PDP decision and every record shares it. That is the
    /// arrangement in which a mis-routed index is invisible unless the
    /// per-record outcomes are asserted individually.
    fn submission(idem: &str) -> CreateUsageRecord {
        submission_for("rsc-period", idem)
    }

    fn submission_for(resource_id: &str, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
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

        // The plugin echoes back only the two records that survive the
        // precondition; a batch that dispatched three (or the wrong two)
        // would trip the service's dispatched-vs-returned length invariant.
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
    /// `HappyPathPlugin` is deliberately left UNPROGRAMMED here — an empty
    /// dispatch would surface as a batch-level `Err`, so `Ok(..)` is direct
    /// evidence the SPI was skipped rather than called with nothing. It also
    /// pins that the tail's "every slot populated" guard is satisfied by a
    /// converted-and-rejected slot, not by an invariant-breach `Internal`.
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
    /// The interleaving is the whole point. The period-rejected submission
    /// never enters `derived`, so any pass that re-`enumerate()`s the
    /// surviving vector shifts the denial from input index 2 onto index 1 —
    /// a slot that is already populated with the period error. A grouped
    /// layout would let the shifted denial land on an empty tail slot and
    /// surface only as an invariant breach; here it OVERWRITES a populated
    /// slot and promotes the denied record to accepted, which is what the
    /// per-index assertions below distinguish.
    #[tokio::test]
    async fn all_three_per_record_outcomes_coexist_at_their_own_input_indices() {
        let plugin = HappyPathPlugin::new();

        // Index 0: valid, permitted. Index 1: valid tuple but an inverted
        // period. Index 2: valid period, denied tuple (`rsc-DENY`).
        let accepted = submission_for("rsc-OK", "idem-three-way-accepted");
        let mut bad_period = submission_for("rsc-OK", "idem-three-way-period");
        bad_period.window_end = bad_period.window_start - time::Duration::seconds(1);
        let denied = submission_for("rsc-DENY", "idem-three-way-denied");

        // Exactly ONE record survives to the SPI. Programming one response
        // is itself a guard: a batch that dispatched two would trip the
        // service's dispatched-vs-returned length invariant and fail the
        // `expect` below rather than reach the assertions.
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

// The kind/op compatibility rule (`require_op_allowed_for_kind` /
// the deleted aggregation-op allow-list) is gone, not relocated: a meter declares
// exactly one fold and a caller cannot choose one at all, so there is no
// (op, kind) pair left to validate. This module replaces
// `aggregate_op_kind_enforcement_tests`: the fold-serving and fail-closed
// tests below are the ones the task plan calls for; the bucket-cap tests
// are carried over (reworked onto the resolver-backed builder) because
// `MAX_AGGREGATION_BUCKETS` enforcement is an unrelated concern that
// survives this change unchanged.
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
        recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    /// Build a `Service` + [`RecordingPlugin`] spy over `source`, wired
    /// against the fixed-tenant PDP fake the aggregate surface requires
    /// (see [`recording_plugin_resolver`]).
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

// The kind/op compatibility rule is gone from ingestion too, the same way
// Task 8 removed it from the aggregate path (see the comment above
// `aggregate_declared_fold_tests`): validation runs against the meter's
// resolved declaration instead of a plugin-owned catalog row, and the
// per-distinct-`gts_id` catalog fan-out in `create_usage_records` is now a
// resolver fan-out over the same Type Resolver the aggregate path uses.
mod ingestion_declared_type_tests {
    use std::collections::BTreeMap;

    use std::sync::Arc;

    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MetadataKey, MeterTypeId, ResourceRef,
        UsageCollectorPluginV1,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::ports::declarations::DeclarationSource;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_counting, fake_declaration_source_not_found,
        fake_declaration_source_with_metadata, projected, recording_plugin_resolver,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// Build a `Service` + [`RecordingPlugin`] spy over `source` at the
    /// default metadata size cap, wired against the fixed-tenant PDP fake
    /// (see [`recording_plugin_resolver`]).
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
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new("rsc-ingestion-declared", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(format!("idem-{}", Uuid::new_v4()))
                .expect("valid idempotency key"),
            corrects_id: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
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

        let err = svc
            .create_usage_record(&ctx(), record)
            .await
            .expect_err("an undeclared key must be rejected before persistence");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
        // The sketch's `spy.calls()` counts `query_aggregated_usage_records`
        // dispatches only (see `HappyPathPlugin::calls`'s doc comment) — the
        // wrong signal for a create-path rejection. `last_create_record_input`
        // is the correct "did the create SPI get dispatched at all" probe.
        assert!(
            spy.last_create_record_input().is_none(),
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
        spy.set_create_record(projected(&record));

        svc.create_usage_record(&ctx(), record)
            .await
            .expect("a declared metadata key must be accepted");
        assert!(
            spy.last_create_record_input().is_some(),
            "an accepted entry must reach the persist SPI",
        );
    }

    #[tokio::test]
    async fn ingestion_fails_closed_when_the_type_does_not_resolve() {
        // 2.1 fail-closed: an unresolvable reference is rejected, never
        // admitted unvalidated to protect ingestion availability.
        let source = fake_declaration_source_not_found();
        let (svc, spy) = service_with_recording_plugin(source);

        assert!(
            svc.create_usage_record(&ctx(), valid_create_record())
                .await
                .is_err()
        );
        assert!(
            spy.last_create_record_input().is_none(),
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

    /// End-to-end proof that `Service::metadata_size_cap_bytes` (itself
    /// threaded from `UsageCollectorConfig::metadata_size_cap_bytes` at
    /// bootstrap) actually drives the cap `validate_submit_record_metadata`
    /// enforces — not a lingering hard-coded constant. Complements the
    /// function-level unit tests in `validation_tests.rs`, which pin the
    /// parameter directly but can't prove `Service` forwards its own field.
    #[tokio::test]
    async fn ingestion_honors_a_configured_non_default_metadata_size_cap() {
        let source = fake_declaration_source_with_metadata(&["blob"]);
        let configured_cap = 16;
        let (svc, spy) = service_with_recording_plugin_and_cap(source, configured_cap);

        let mut record = valid_create_record();
        record
            .metadata
            .insert(MetadataKey::new("blob").expect("valid key"), "x".repeat(64));

        let err = svc
            .create_usage_record(&ctx(), record)
            .await
            .expect_err("a payload over the configured cap must be rejected");
        assert!(
            err.to_string().contains(&configured_cap.to_string()),
            "the rejection must name the configured cap, not a hard-coded default: {err}",
        );
        assert!(
            spy.last_create_record_input().is_none(),
            "a rejected entry must not reach the plugin",
        );
    }
}

// Task 12: `CompiledMetadataSchema::declared_keys()` gates the read paths'
// `$filter` / `group_by` surface (Spec §3.11). The pure-function coverage
// (every `ast::Expr` variant, nested-`or`/`not`/`in` rejection, recomputation
// across differing declared-key sets) lives in `query_tests.rs`; this module
// proves the two checks are actually wired into `Service::list_usage_records`
// / `Service::query_aggregated_usage_records` — right position (before
// dispatch), right arguments (the resolved declaration's `declared_keys`).
mod query_admissibility_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{ODataQuery, ast};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationDimension, AggregationResult, MetadataFilter, MetadataKey, MeterTypeId,
        UsageCollectorError, UsageCollectorPluginV1, ValidationReason,
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
    /// predicate. Nothing else is in the filter, so whatever the request is
    /// rejected for is the admissibility gate under test. The mandatory
    /// read range travels beside the filter as a typed parameter, so it
    /// contributes no conjunct here.
    fn filter_naming(reserved_field: &str) -> ODataQuery {
        ODataQuery::from(Some(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier(reserved_field.to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String("x".to_owned()))),
        )))
    }

    /// Build a `Service` + [`RecordingPlugin`] spy over `source`, wired
    /// against the fixed-tenant PDP fake both read paths require (see
    /// [`recording_plugin_resolver`]).
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
        assert_eq!(
            spy.calls(),
            0,
            "a rejected filter must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn list_rejects_a_filter_naming_a_covered_period_bound() {
        // Both bounds are filterable-schema fields — they have to be, for
        // the `(window_end, id)` keyset and the cursor tokens to resolve to
        // a column — so the reserved-field guard is the only thing between
        // a `$filter` predicate and the plugin. A case-varied spelling now
        // resolves as a legitimate field rather than dead-ending as an
        // unknown one, which is why the guard compares case-insensitively.
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
        use toolkit_odata::{Page as ODataPage, PageInfo};

        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        spy.set_list_usage_records_response(ODataPage {
            items: vec![],
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: 1000,
            },
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
        // Two independently-resolved declarations for the same meter id,
        // one declaring `region` and one not: the SAME dimension is
        // rejected against the first and accepted against the second,
        // proving the admissible set comes from the declaration resolved
        // for *this* request rather than anything fixed at service
        // construction (or cached independently of the declaration).
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

    // ── metadata_filter admissibility (Spec §3.11's third gated surface) ───
    //
    // `metadata_filter` is the dynamic-key side channel that exists precisely
    // because `toolkit-odata`'s grammar cannot express a filter over a JSON
    // map key — it never flows through `$filter`, so it needs its own
    // declared-keys gate on both read paths, wired at the same point as the
    // `$filter` / `group_by` checks above.

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
        use toolkit_odata::{Page as ODataPage, PageInfo};

        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy.set_list_usage_records_response(ODataPage {
            items: vec![],
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: 1000,
            },
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
        // Two independently-resolved declarations for the same meter id,
        // one declaring `region` and one not: the SAME `metadata_filter` key
        // is rejected against the first and accepted against the second —
        // the `group_by` recomputation proof mirrored onto the third gated
        // surface. Exercised on the list path since that's the surface
        // whose declaration resolution is new in this task.
        use toolkit_odata::{Page as ODataPage, PageInfo};

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
        spy_after.set_list_usage_records_response(ODataPage {
            items: vec![],
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: 1000,
            },
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

    #[tokio::test]
    async fn list_now_fails_closed_when_the_type_does_not_resolve() {
        // Behavioural change introduced by this task: `list_usage_records`
        // previously resolved no declaration at all (it has no `group_by`),
        // so an unresolvable `gts_type_id` reached the plugin unchecked.
        // Gating `metadata_filter` against the declared keys requires
        // resolving one, so an unresolvable type now fails closed here as a
        // pre-dispatch 404 — mirrors
        // `aggregate_fails_closed_when_the_type_does_not_resolve` in
        // `aggregate_declared_fold_tests`.
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
}

// The mandatory read range is a typed `TimeRange` parameter on both read
// paths, never a `$filter` conjunct (DESIGN §3.3 rule 5). Two things need
// pinning, and neither is visible in a status code: that the range the
// caller supplied is the one the SPI is handed, and that a caller supplying
// no `$filter` at all is now a complete request. A range dropped between
// the gateway and the plugin is an unbounded scan that still answers `Ok`,
// so every assertion here is on what the plugin received.
mod read_path_time_range_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{ODataQuery, Page as ODataPage};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{AggregationResult, MeterTypeId, TimeRange, UsageCollectorPluginV1};

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

    /// A `Service` plus the [`RecordingPlugin`] spy behind it, over a
    /// declaration that declares no metadata keys (nothing here filters on
    /// one) and the fixed-tenant PDP fake both read paths require.
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
        spy.set_list_usage_records_response(ODataPage::empty(0));

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
        // The forwarding tests above would both pass against a service that
        // hard-coded a single range, because they only ever pass one. Two
        // dispatches with two different ranges, through the same service and
        // the same spy, is what rules that out.
        let (svc, spy) = svc_and_spy();

        let first = test_time_range();
        let second = TimeRange::new(
            first.upper_exclusive(),
            first.upper_exclusive() + time::Duration::hours(1),
        )
        .expect("the adjacent hour is a valid range");
        assert_ne!(first, second, "precondition: the two ranges differ");

        spy.set_list_usage_records_response(ODataPage::empty(0));
        svc.list_usage_records(&ctx(), meter_id(), first, &ODataQuery::default(), &[])
            .await
            .expect("first list succeeds");
        assert_eq!(spy.last_list_time_range(), Some(first));

        spy.set_list_usage_records_response(ODataPage::empty(0));
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

    /// A `$filter` that constrains something other than time — the shape a
    /// caller wanting both a predicate and a range now sends. The
    /// forwarding tests above pass `ODataQuery::default()`, so between them
    /// the pair covers both "no `$filter` at all" and "a `$filter` naming
    /// no time predicate". Before this slice the second was a 400 too,
    /// because the retired guard read the window out of this same slot.
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
        spy.set_list_usage_records_response(ODataPage::empty(0));

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

// DESIGN §3.1 "Order admissibility" allocates the keyset to the Query
// Gateway so the plugin *always* receives a gap-free, uniform-direction,
// never-null order — §3.2's whole reason for a single gateway is that
// enforcement is uniform across the SDK and REST. These tests cover the
// surface no REST test can reach: a caller who holds a `Service` and hands
// it an `ODataQuery` of their own construction. An unfloored order is not
// visible in a status code — it is a keyset the plugin cannot continue, or
// one that silently drops rows at a page boundary — so every assertion
// here is on the order the plugin received.
mod read_path_keyset_floor_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, Page as ODataPage, SortDir};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        MeterTypeId, UsageCollectorError, UsageCollectorPluginV1, ValidationReason,
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

    /// Same shape as `read_path_time_range_tests`: a `Service` over the
    /// [`RecordingPlugin`] spy, a declaration declaring no metadata keys,
    /// and the fixed-tenant PDP fake both read paths require.
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
        // The case no REST test can reach, and the reason the floor is in
        // the domain rather than in `prepare_list_query`: an in-process
        // caller hands the service an `ODataQuery::default()`, whose order
        // is empty. The SPI documents `query.order` as populated, so
        // without a domain-side floor the plugin would receive a slot the
        // contract says is filled.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));

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
        // survives and only gains the canonical fields it does not name —
        // both, here — in its own direction, so the row-value comparison
        // stays uniform.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));

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
        // Mixed directions and a nullable leading key are both unsound
        // keysets that appending a suffix cannot fix, so the service
        // refuses before dispatch rather than forwarding them. `created_at`
        // is in the same bucket now: it is not a record attribute, and the
        // classification is a fail-closed allowlist.
        for order in [
            vec![("resource_id", SortDir::Asc), ("tenant_id", SortDir::Desc)],
            vec![("subject_id", SortDir::Asc)],
            vec![("created_at", SortDir::Asc)],
        ] {
            let (svc, spy) = svc_and_spy();
            spy.set_list_usage_records_response(ODataPage::empty(0));

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
    /// `f` is minted the same way: a conforming plugin echoes the
    /// `filter_hash` it was dispatched with, which is
    /// [`read_fingerprint`] over the caller's query and range. Without it
    /// every fixture here would be refused as a cursor bound to another
    /// query, and these tests are about the order.
    fn cursor_request_ordered_by(keys: &[(&str, SortDir)]) -> ODataQuery {
        let signed = query_ordered_by(keys).order.to_signed_tokens();
        let mut query = ODataQuery::new();
        query.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        query.cursor = Some(CursorV1 {
            k: query
                .order
                .0
                .iter()
                .map(|_| "boundary".to_owned())
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
        // The service picks which mode of the floor to run, so this is
        // where "a continuation is checked, not extended" can actually
        // fail: `require_continuation_keyset` takes `&ODataQuery` and
        // cannot mutate, but nothing stops the service calling
        // `establish_keyset_order` instead. The order here is what a
        // conforming plugin mints — a floored order, round-tripped through
        // the token's signed keys — and the plugin must receive it with
        // the same keys, directions and width, which is what keeps the
        // continuation predicate aligned with the token's boundary values.
        let keys = [
            ("resource_id", SortDir::Desc),
            ("window_end", SortDir::Desc),
            ("id", SortDir::Desc),
        ];
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));

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
    }

    #[tokio::test]
    async fn a_continuation_the_floor_would_have_to_widen_never_reaches_the_plugin() {
        // The deliberate choice this pins: on a cursor request the order is
        // required to already be a sound keyset, not extended into one.
        // Appending `window_end` to a one-key token would hand the plugin
        // a two-key order against one boundary value — a misaligned
        // continuation, which is a silently wrong page. Refusing is the
        // observable failure. Only a forged token or a non-conforming
        // plugin gets here.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));

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
            UsageCollectorError::InvalidArgument { field, reason, .. } => {
                assert_eq!(
                    field, "cursor",
                    "the 400 must blame the token, not an $orderby the caller \
                     cannot send alongside a cursor",
                );
                assert_eq!(reason, ValidationReason::InvalidCursor);
            }
            other => panic!("expected InvalidArgument on cursor, got {other:?}"),
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
/// `CursorV1::f` exists so a caller who changes their query between pages
/// is refused rather than served a continuation minted over a different row
/// set. Four inputs decide that row set: the caller's `$filter` and the
/// three typed parameters `gts_type_id`, the read range and
/// `metadata_filter`. None travels inside `$filter` any more, so each has
/// to enter the fingerprint explicitly — or page 2 of a January query
/// happily continues from a cursor minted over February, or against
/// another meter, and a wrong page is an `Ok` nothing else in the stack
/// notices.
///
/// Asserted through the service rather than the handler on purpose:
/// `ODataQuery::filter_hash` is `None` for an in-process caller and
/// `toolkit_odata::validate_cursor_against` skips its comparison when
/// either side is `None`, so this is exactly the caller an edge-only check
/// leaves unprotected — and the one no REST test can reach.
mod read_path_cursor_fingerprint_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, Page as ODataPage, SortDir};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        MetadataFilter, MeterTypeId, TimeRange, UsageCollectorError, UsageCollectorPluginV1,
        ValidationReason,
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

    /// Same shape as `read_path_keyset_floor_tests`.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        svc_and_spy_declaring(&[])
    }

    /// The same, over a declaration declaring exactly `declared` metadata
    /// keys — a `metadata_filter` naming an undeclared key is refused by
    /// the query-surface gate long before the fingerprint is compared, so
    /// the metadata shapes need their keys declared to reach the subject.
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

    /// Page one of `query` over `range`, dispatched for real, returning the
    /// `filter_hash` the plugin was handed.
    ///
    /// This is the mint leg of every round trip below, and it is a real
    /// dispatch rather than a `read_fingerprint` call so the value under
    /// test is the one the service actually put on the wire to the plugin.
    /// A conforming plugin copies it into `next_cursor.f`.
    async fn mint_fingerprint(
        meter: &MeterTypeId,
        range: TimeRange,
        query: &ODataQuery,
        metadata: &[MetadataFilter],
        declared: &[&str],
    ) -> String {
        let (svc, spy) = svc_and_spy_declaring(declared);
        spy.set_list_usage_records_response(ODataPage::empty(0));
        svc.list_usage_records(&ctx(), meter.clone(), range, query, metadata)
            .await
            .expect("page one is a complete request");
        spy.last_list_query()
            .expect("the plugin MUST have been dispatched")
            .filter_hash
            .expect("every dispatch MUST carry the fingerprint the plugin mints")
    }

    /// `query` again, now carrying the canonical continuation a plugin
    /// would have minted from `fingerprint`.
    ///
    /// The order round-trips through `to_signed_tokens` /
    /// `from_signed_tokens` exactly as `prepare_list_query` step 3 does, so
    /// the cursor under test is decoded rather than assembled.
    /// `query` again, carrying a continuation bound to `keys` verbatim —
    /// no flooring, so the order can be made deliberately unsound.
    ///
    /// The order still round-trips through the signed tokens, exactly as
    /// the handler rebuilds it, so what is under test is a decoded order
    /// rather than one set alongside the token.
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

    fn assert_query_mismatch(err: &UsageCollectorError) {
        match err {
            UsageCollectorError::InvalidArgument { field, reason, .. } => {
                assert_eq!(
                    field, "cursor",
                    "the 400 must blame the token, not an $orderby the caller \
                     cannot send alongside a cursor",
                );
                assert_eq!(*reason, ValidationReason::FilterMismatch);
            }
            other => panic!("expected InvalidArgument on cursor, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_cursor_minted_under_the_same_range_is_served() {
        // The half a careless "just refuse every cursor" edit would break,
        // and the one that proves the value round-trips: the service
        // dispatches a fingerprint, a conforming plugin mints it into
        // `next_cursor.f`, and the follow-up request recomputes the same
        // string from its own `$filter` and range.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));
        svc.list_usage_records(
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
        assert_eq!(
            dispatched.filter_hash,
            Some(fingerprint),
            "a continuation dispatch must carry the fingerprint too: the \
             plugin mints page THREE's cursor from whatever it was handed \
             here, so assigning it on the first-page branch alone breaks \
             pagination one page later than any two-page test would notice",
        );
    }

    #[tokio::test]
    async fn a_cursor_minted_under_a_different_range_never_reaches_the_plugin() {
        // The whole reason this task exists. Same caller, same `$filter`,
        // a different range — which used to be a `$filter` conjunct and so
        // was covered by the hash for free.
        let caller = query_with_filter("resource_id eq 'r1'");
        let january = test_time_range();
        let february = range_at(60 * 60 * 24 * 31 * 1_000_000_000);
        assert_ne!(january, february, "precondition: two different ranges");
        let fingerprint = mint_fingerprint(&meter_id(), january, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));
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
        // The truncation hole the plan caught before the code existed: if
        // the range rendering reused the identity derivation's fixed
        // six-digit-microsecond form, these two ranges would fingerprint
        // identically and this cursor would be served.
        let caller = query_with_filter("resource_id eq 'r1'");
        let coarse = range_at(0);
        let nudged = range_at(1);
        let fingerprint = mint_fingerprint(&meter_id(), coarse, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));
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
        // The property the range was added to, not a replacement for it.
        // `$filter` was already covered before this slice, and an
        // implementation that fingerprinted the range alone would lose it
        // silently while every range test above stayed green.
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
        spy.set_list_usage_records_response(ODataPage::empty(0));
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
        // `validate_cursor_against` skips its own comparison whenever
        // either side is absent. The cursor is caller-supplied JSON, so an
        // absent `f` is not evidence of a matching query — and admitting
        // it would leave the whole check optional at the caller's
        // discretion.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let mut page_two = continuation_of(&caller, "unused");
        page_two.cursor.as_mut().expect("cursor").f = None;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &page_two, &[])
            .await
            .expect_err("an unbound cursor must be refused");

        assert_query_mismatch(&err);
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_cursor_minted_against_another_meter_never_reaches_the_plugin() {
        // The widest version of the wrong-page failure: `gts_type_id` is a
        // typed parameter, so no filter hash ever covered it, and without
        // it in the fingerprint a caller can continue a cursor against a
        // different meter and be served rows from a set the token knows
        // nothing about.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));
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
        // The other typed row-selector, and the one `$filter` cannot even
        // express — the `toolkit-odata` grammar has no surface for a
        // dynamic JSON-map key, which is why `metadata_filter` is a
        // parameter of its own and why a filter hash reaches none of it.
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
        spy.set_list_usage_records_response(ODataPage::empty(0));
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
        // The over-rejection guard, at the service rather than in a
        // rendering unit test: a REST caller's repeated `metadata.<key>`
        // parameters arrive in query-string order with duplicates intact,
        // so page two of an unchanged query can legitimately carry the
        // same value set in a different spelling. That is the same query
        // and MUST still be served — a fingerprint that read the slice
        // verbatim would refuse it.
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
        spy.set_list_usage_records_response(ODataPage::empty(0));
        svc.list_usage_records(
            &ctx(),
            meter_id(),
            range,
            &continuation_of(&caller, &fingerprint),
            // Entries swapped, values re-ordered, and one value repeated —
            // all three are things a caller's own query string does.
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
    async fn a_doubly_defective_cursor_is_refused_for_its_order_first() {
        // The check order is a documented fact (`require_continuation_keyset`
        // says structure is checked before relevance) and it is
        // caller-visible: a token that is BOTH bound to an unsound keyset
        // and minted over another query gets `INVALID_CURSOR` today and
        // would get `FILTER_MISMATCH` if the two checks were swapped —
        // different reason code on the wire, and a different
        // `error_category` on `uc_query_requests_total` with it. Nothing
        // else in the suite distinguishes the order, because every other
        // cursor test is defective in exactly one way.
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
        spy.set_list_usage_records_response(ODataPage::empty(0));
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &doubly_defective, &[])
            .await
            .expect_err("a doubly-defective token must be refused");

        match err {
            UsageCollectorError::InvalidArgument { field, reason, .. } => {
                assert_eq!(field, "cursor");
                assert_eq!(
                    reason,
                    ValidationReason::InvalidCursor,
                    "the ORDER defect must be reported: it is checked first, \
                     and a swap would silently re-label this as FILTER_MISMATCH",
                );
            }
            other => panic!("expected InvalidArgument on cursor, got {other:?}"),
        }
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn the_dispatched_fingerprint_is_computed_from_the_callers_filter_not_the_composed_one() {
        // `compose_query_with_scope` AND-merges the server-injected PDP
        // scope into `$filter` and deliberately preserves the caller's
        // `filter_hash`, for the reason it documents at length: the scope
        // is not caller-controlled, so a fingerprint over the composed
        // filter embeds a value the next request's recomputation can never
        // reproduce. That failure is silent until a scope exists, which is
        // why this asserts the value rather than only the round trip — the
        // round trip is stable either way, since the PDP fake here returns
        // the same scope every call.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));
        svc.list_usage_records(&ctx(), meter_id(), range, &caller, &[])
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
        assert_eq!(
            dispatched.filter_hash,
            Some(read_fingerprint(&meter_id(), range, &caller, &[])),
            "the fingerprint the plugin mints MUST be the caller's filter \
             and range, never the composed filter",
        );
    }
}
