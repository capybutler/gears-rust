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
use usage_collector_sdk::{
    UsageCollectorPluginSpecV1, UsageCollectorPluginV1, UsageRecordFilterField,
};

use super::*;
use crate::domain::ports::declarations::DeclarationSource;
use crate::domain::test_support::{HappyPathPlugin, MockPlugin, UnreachableResolver, enforcer_for};

/// Dummy enforcer for tests that never reach the PDP path
/// (binding / plugin-host tests). An unreachable PDP transport never matters
/// when no authz call is made.
fn dummy_enforcer() -> PolicyEnforcer {
    enforcer_for(Arc::new(UnreachableResolver))
}

/// The scope [`target_pinned_read_filter`] builds must be one a conforming
/// backend can render, and must pin the row it claims to pin.
///
/// This is a regression test with a field report behind it. The scope used to
/// be a bare `true` literal, which reads as "no narrowing" and is refused in
/// a boolean position by BOTH `convert_expr_to_filter_node`
/// (`FilterError::BareLiteral`) and the SDK's own reference implementation
/// (`contract/reference.rs`). Every invalidation submitted to the `TimescaleDB`
/// backend therefore came back `500 Internal("invalid read predicate")`.
///
/// **This test alone would not have caught that**, and saying so is the point:
/// it calls the helper directly and never observes what the two call sites
/// pass, so reverting either of them would leave it green. The assertions that
/// watch the call sites are
/// [`assert_translatable_scope`]'s, one per path — see its doc for why both
/// are needed and what each one covers.
#[test]
fn the_invalidation_target_scope_translates_for_a_conforming_backend() {
    let target = Uuid::from_u128(0xF11E);
    let scope = target_pinned_read_filter(target);

    let node = toolkit_odata::filter::convert_expr_to_filter_node::<UsageRecordFilterField>(&scope)
        .expect("the invalidation-target scope must be translatable by any conforming plugin");

    // Destructured rather than substring-matched on a `Debug` rendering: the
    // claim is `id eq <target>` and nothing else, and a rendering match would
    // also accept, say, `tenant_id eq <target>`. `field.name()` rather than a
    // variant pattern, because the enum is macro-derived and its variant
    // spelling is not the published contract - the field name is.
    // `ODataValue` implements no `PartialEq`, so the value arm is a `matches!`
    // guard rather than an equality assertion.
    match node {
        toolkit_odata::filter::FilterNode::Binary { field, op, value } => {
            assert_eq!(
                toolkit_odata::filter::FilterField::name(&field),
                usage_collector_sdk::RECORD_ID_FIELD
            );
            assert_eq!(op, toolkit_odata::filter::FilterOp::Eq);
            assert!(
                matches!(value, ast::Value::Uuid(id) if id == target),
                "the scope must compare `id` against the target uuid, got {value:?}"
            );
        }
        other => panic!("the scope must be one `id eq <uuid>` comparison, got {other:?}"),
    }

    // The negative half, and it is what keeps the positive one from going
    // quiet: the shape this replaced has to still be refused. If the
    // converter ever grows a bare-literal arm, the assertion above would pass
    // for a scope that no longer needed replacing and this test would stop
    // saying anything.
    assert!(
        toolkit_odata::filter::convert_expr_to_filter_node::<UsageRecordFilterField>(
            &ast::Expr::Value(ast::Value::Bool(true))
        )
        .is_err(),
        "a bare literal in a boolean position is what made this fix necessary"
    );
}

/// Assert that the scope the service last handed `get_usage_record` is one a
/// conforming backend can translate.
///
/// **Called once per call site, and both calls are load-bearing.** The two
/// invalidation-target pre-reads are separate expressions in separate
/// functions - `resolve_invalidation_targets` for the batch path,
/// `Service::create_usage_record_inner` for the single-record one - so one
/// assertion covers one of them and says nothing about the other. The E2E
/// suite cannot close the gap either: the gear publishes no single-record
/// POST, so `create_usage_record` is reachable only through the in-process
/// `ClientHub` path (`local_client.rs`) and no HTTP test can drive it.
///
/// The question is deliberately "does it translate", not "what shape is it":
/// a future scope that renders differently but a backend can still serve is
/// fine, and that is exactly the distinction a `Debug`-string assertion
/// cannot draw. [`HappyPathPlugin::last_get_scope`] renders; this reads
/// [`HappyPathPlugin::last_get_scope_expr`] and puts the real converter
/// behind the answer.
///
/// **It reads the LAST capture, so it only speaks for the whole test when
/// there was one dispatch.** Both current callers sit immediately after an
/// assertion pinning `get_usage_record_calls()` to exactly 1, which is what
/// makes "the last scope" mean "the only scope". A test over several distinct
/// targets must pin the count too, or this passes on one scope and says
/// nothing about the others.
///
/// **If a third call site ever appears, reconsider the layer.** Putting the
/// converter inside [`crate::domain::test_support::TargetLookupDouble`]'s
/// lookup, at capture time, would refuse an untranslatable scope from every
/// call site including ones not yet written, and the per-site cost would stop
/// compounding. It is deliberately not done at two: a double that enforces is
/// no longer a double that records, some test may one day want to observe a
/// deliberately bad scope, and with two compile-time-known call sites naming
/// them is more informative than a blanket refusal that names none.
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
        crate::domain::test_support::default_covered_period_bounds(),
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

    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef, SubjectRef,
        UsageCollectorPluginV1, UsageRecord,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        recent_window_end, recent_window_start,
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
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
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
            crate::domain::test_support::default_covered_period_bounds(),
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

    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef,
        USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginV1, UsageRecord,
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
            gts_type_id: meter_id_for(gts),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-gts-dedup", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
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
            value: input.value,
            idempotency_key: input.idempotency_key.clone(),
            origin: RecordOrigin::Live,
            invalidation: input.invalidation.clone(),
            window_start: input.window_start,
            window_end: input.window_end,
        }
    }

    // The "5 records, identical gts_type_id → exactly one resolution" case is
    // now covered by `ingestion_declared_type_tests::a_batch_resolves_each_distinct_type_once`
    // (which uses `CountingDeclarationSource`) — deleted here rather than
    // duplicated.

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

// ── invalidation-target pre-check in `create_usage_records` ───────────────
//
// Pins the intra-batch dedup described in
// `cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2`
// instructions `inst-algo-semantics-l1-dedup` and
// `inst-algo-semantics-l1-bounded-fanout` — entries naming the same target
// MUST collapse to a single `get_usage_record` SPI round-trip, and an
// ordinary measurement MUST NOT trigger a target read at all — plus the
// per-index pairing the fan-out is the only place that can break.
#[cfg(test)]
mod invalidation_target_batch_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        ConflictReason, CreateUsageRecord, IdempotencyKey, Invalidation, MetadataKey, MeterTypeId,
        ReasonCode, ResourceRef, USAGE_RECORD_RESOURCE, UsageCollectorError,
        UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord, ValidationReason,
    };
    use uuid::Uuid;

    use super::assert_translatable_scope;
    use crate::domain::Service;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, counter_sum_with_label,
        fake_declaration_source_with_fold, fake_declaration_source_with_metadata, projected,
        recent_window_end, recent_window_start,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] wired with an arbitrary working declaration —
    /// these tests exercise the target pre-check, not the declaration.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// The base measurement every fixture here is shaped from. An
    /// invalidation is a faithful copy of it, so a withdrawal is built by
    /// changing exactly the two permitted departures — the idempotency key
    /// and the `invalidation` field itself.
    fn ordinary_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-target", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(10),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn withdrawal_of(tenant_id: Uuid, target: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            invalidation: Some(Invalidation {
                target,
                reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
            }),
            ..ordinary_record(tenant_id, idem)
        }
    }

    /// The persisted entry a withdrawal built by [`withdrawal_of`] copies
    /// faithfully: same caller-supplied fields, its own identity and its
    /// own idempotency key.
    fn target_row(tenant_id: Uuid, id: Uuid) -> UsageRecord {
        UsageRecord {
            id,
            idempotency_key: IdempotencyKey::new("idem-target").expect("valid idempotency key"),
            ..projected(&ordinary_record(tenant_id, "idem-target"))
        }
    }

    /// Five withdrawals of one target MUST collapse to a single
    /// `get_usage_record` SPI round-trip. The count in the name is left
    /// open because the body's is five and a name that says "two" is a
    /// claim a reader would have to check against the body to disbelieve.
    ///
    /// The read is what the dedup buys; the store still admits at most one
    /// of them, and that rejection is the plugin's rather than a second
    /// read's.
    #[tokio::test]
    async fn withdrawals_of_one_target_share_a_single_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x501);
        let target = Uuid::from_u128(0x601);
        plugin.set_get_record(target_row(tenant_id, target));

        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| withdrawal_of(tenant_id, target, &format!("idem-w-{i}")))
            .collect();
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

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

        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
    }

    /// Three distinct targets MUST produce three `get_usage_record`
    /// dispatches, for exactly those three ids.
    #[tokio::test]
    async fn distinct_targets_each_cost_their_own_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x502);

        let target_a = Uuid::from_u128(0x602);
        let target_b = Uuid::from_u128(0x603);
        let target_c = Uuid::from_u128(0x604);
        for target in [target_a, target_b, target_c] {
            plugin.set_get_record_for(target, target_row(tenant_id, target));
        }

        let input = vec![
            withdrawal_of(tenant_id, target_a, "idem-A"),
            withdrawal_of(tenant_id, target_b, "idem-B"),
            withdrawal_of(tenant_id, target_c, "idem-C"),
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

    /// An ordinary measurement MUST cost no target lookup at all.
    ///
    /// Asserting the *absence* of the dispatch is the point: a gateway that
    /// looked up unconditionally would pass every acceptance test in this
    /// module while doubling the plugin traffic of the common path.
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

    /// A target that resolves to nothing MUST reject every entry naming
    /// it, at its own input index, while an entry naming a resolvable
    /// target in the same batch is still accepted.
    ///
    /// The rejections sit at indices 0 and 2 with the acceptance between
    /// them, so a projection that wrote every outcome to one slot leaves
    /// another empty and cannot pass.
    #[tokio::test]
    async fn an_unresolvable_target_rejects_every_entry_naming_it() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x504);

        let good = Uuid::from_u128(0x605);
        let missing = Uuid::from_u128(0x606);
        plugin.set_get_record_for(good, target_row(tenant_id, good));
        plugin.set_get_usage_record_not_found(missing);

        let input = vec![
            withdrawal_of(tenant_id, missing, "idem-bad-0"),
            withdrawal_of(tenant_id, good, "idem-good-0"),
            withdrawal_of(tenant_id, missing, "idem-bad-1"),
        ];

        // Only the resolvable one reaches the persist SPI; program one
        // accepted response.
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

    /// Two withdrawals of **different** targets, resolved in one batch,
    /// MUST each be told about the target *they* named.
    ///
    /// This is the pairing the comparator cannot check for itself: it is
    /// handed a row and a reference and trusts they belong together, so the
    /// fan-out — keyed by target, projected back by input index — is the
    /// only place the two can come apart. The rows are deliberately
    /// different *kinds* of wrong, so a swap of either the key or the index
    /// changes which reason lands where, not merely which uuid it carries.
    #[tokio::test]
    async fn each_rejection_names_the_target_its_own_entry_sent() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x505);

        // Target A is itself a withdrawal — the no-invalidation-of-an-
        // invalidation rule.
        let target_a = Uuid::from_u128(0x607);
        let mut row_a = target_row(tenant_id, target_a);
        row_a.invalidation = Some(Invalidation {
            target: Uuid::from_u128(0x60F),
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        });
        plugin.set_get_record_for(target_a, row_a);

        // Target B is an ordinary entry the submission copies unfaithfully.
        let target_b = Uuid::from_u128(0x608);
        let mut row_b = target_row(tenant_id, target_b);
        row_b.value = rust_decimal::Decimal::from(999);
        plugin.set_get_record_for(target_b, row_b);

        let input = vec![
            withdrawal_of(tenant_id, target_a, "idem-pair-a"),
            withdrawal_of(tenant_id, target_b, "idem-pair-b"),
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
            Err(UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationTargetNotRecord,
                resource_name,
                ..
            }) => assert_eq!(
                resource_name.as_deref(),
                Some(target_a.to_string().as_str()),
                "index 0 MUST be told about the target IT named",
            ),
            other => panic!("index 0 MUST be refused as a non-record target, got {other:?}"),
        }
        match results[1].as_ref() {
            Err(UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationFieldMismatch,
                field,
                resource_name,
                ..
            }) => {
                assert_eq!(field, "value", "the field that differs MUST be named");
                assert_eq!(
                    resource_name.as_deref(),
                    Some(target_b.to_string().as_str()),
                    "index 1 MUST be told about the target IT named",
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
        let target = Uuid::from_u128(0x609);

        // The declaration declares `region` and nothing else, so `zone` is
        // an undeclared key and fails the closed-shape check.
        let mut row = target_row(tenant_id, target);
        row.value = rust_decimal::Decimal::from(999);
        plugin.set_get_record_for(target, row);

        let mut submission = withdrawal_of(tenant_id, target, "idem-both-broken");
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

    /// The store's own rule: a target that already carries a withdrawal is
    /// refused by the plugin, and the gateway lifts it verbatim rather than
    /// pre-reading for it.
    #[tokio::test]
    async fn a_plugin_already_invalidated_rejection_is_lifted_as_a_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x507);
        let target = Uuid::from_u128(0x60A);
        let existing = Uuid::from_u128(0x60B);
        plugin.set_get_record_for(target, target_row(tenant_id, target));
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::AlreadyInvalidated {
            id: target,
            invalidated_by: existing,
        })]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.already_invalidated.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(tenant_id, target, "idem-second")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                reason: ConflictReason::AlreadyInvalidated,
                name,
                detail,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                assert!(detail.contains(&existing.to_string()));
            }
            other => panic!("at-most-one is the store's rule to report, got {other:?}"),
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
        let target = Uuid::from_u128(0x60C);
        plugin.set_get_usage_record_transient("target store timed out", Some(7));

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.transient.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(tenant_id, target, "idem-transient")],
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
    /// target pre-check pushes its verified entries out of the input-order
    /// loop and onto the end of `eligible`.
    ///
    /// Nothing user-visible depends on it — per-entry results are routed
    /// back by input index either way — which is exactly why it needs its
    /// own test: the property lives entirely in what the plugin receives,
    /// and `last_create_records_input` is the only place it is observable.
    /// The invalidation is at index 0 so removing the sort reverses the
    /// pair rather than leaving it untouched.
    #[tokio::test]
    async fn the_plugin_sees_the_batch_in_submission_order() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50A);
        let target = Uuid::from_u128(0x610);
        plugin.set_get_record_for(target, target_row(tenant_id, target));

        let input = vec![
            withdrawal_of(tenant_id, target, "idem-order-withdrawal"),
            ordinary_record(tenant_id, "idem-order-plain"),
        ];
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.order.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert!(results.iter().all(Result::is_ok), "{results:?}");

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
             plugin is handed",
        );
    }

    /// A transient on one entry's target MUST NOT abandon the entries
    /// behind it in the pending list.
    ///
    /// The generic backend-fault arm projects per entry and continues; a
    /// version that returned instead would leave every later slot unfilled.
    /// The unreadable target is first, so the surviving entries are exactly
    /// the ones a `return` would drop.
    #[tokio::test]
    async fn a_transient_on_one_target_does_not_abandon_the_rest_of_the_batch() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50B);

        let unreadable = Uuid::from_u128(0x611);
        let readable = Uuid::from_u128(0x612);
        plugin.set_get_usage_record_transient_for(unreadable, "target store timed out", Some(7));
        plugin.set_get_record_for(readable, target_row(tenant_id, readable));

        let input = vec![
            withdrawal_of(tenant_id, unreadable, "idem-iso-unreadable"),
            withdrawal_of(tenant_id, readable, "idem-iso-readable"),
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
    /// The label is captured per input index before the pipeline consumes
    /// the submissions and re-joined to the outcomes by `zip` afterwards;
    /// that join is the only place a label can come apart from the entry it
    /// describes, and no single-emit test reaches it.
    #[tokio::test]
    async fn a_batch_labels_each_entry_with_its_own_entry_type() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50C);
        let target = Uuid::from_u128(0x613);
        plugin.set_get_record_for(target, target_row(tenant_id, target));

        let input = vec![
            ordinary_record(tenant_id, "idem-label-plain"),
            withdrawal_of(tenant_id, target, "idem-label-withdrawal"),
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

    /// A plugin answering `get_usage_record(a)` with a *different* row MUST
    /// be refused, not believed.
    ///
    /// The fixture is the dangerous shape rather than a convenient one: the
    /// submission is a faithful copy of the row the plugin returns, so
    /// without the check every rule the gateway enforces passes and the
    /// entry is **accepted** — a withdrawal of an entry nothing ever
    /// checked. It is a host-invariant breach rather than a caller fault,
    /// so it surfaces as `Internal`, and the message names only the id the
    /// caller sent: the row came back from an unscoped read and its own
    /// identity must not cross back.
    #[tokio::test]
    async fn a_plugin_answering_with_the_wrong_row_is_refused_not_believed() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50D);
        let requested = Uuid::from_u128(0x614);
        let answered = Uuid::from_u128(0x615);

        // Asked for `requested`, answered with the row for `answered`.
        plugin.set_get_record_for(requested, target_row(tenant_id, answered));

        // The persist SPI is programmed to succeed even though a correct
        // gateway never reaches it. Without that, a gateway that admitted
        // the entry would fail on an unprogrammed SPI instead — the test
        // would still fail, but reporting the wrong defect. Programmed, the
        // failure a reader sees is `Ok(..)`: the false accept itself.
        let submission = withdrawal_of(tenant_id, requested, "idem-wrong-row");
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

    /// One measurement, one good withdrawal and one bad withdrawal MUST
    /// come back index-aligned, with only the bad one rejected — and the
    /// bad one is last, so a projection that wrote to a fixed slot could
    /// not pass.
    #[tokio::test]
    async fn a_mixed_batch_rejects_only_the_entry_that_broke_a_rule() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x509);

        let good = Uuid::from_u128(0x60D);
        let bad = Uuid::from_u128(0x60E);
        plugin.set_get_record_for(good, target_row(tenant_id, good));
        let mut row_bad = target_row(tenant_id, bad);
        row_bad.value = rust_decimal::Decimal::from(999);
        plugin.set_get_record_for(bad, row_bad);

        let input = vec![
            ordinary_record(tenant_id, "idem-mixed-plain"),
            withdrawal_of(tenant_id, good, "idem-mixed-good"),
            withdrawal_of(tenant_id, bad, "idem-mixed-bad"),
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
}

// ─── usage-emission feature (read-by-id) ─────────────────────────────────
//
// `Service::get_usage_record` is the host-side gateway for the read-by-id
// surface of the usage-emission feature. Per DESIGN §3.3 / §3.5
// it authorizes like `list_usage_records` and
// `query_aggregated_usage_records`: a pre-row PDP call
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
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new("idem-happy").expect("valid idempotency key"),
            origin: RecordOrigin::Live,
            invalidation: None,
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
    /// DESIGN §3.3's central guarantee. Asserts the spy actually
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
    /// lifts through the canonical chain to `ServiceUnavailable`.
    #[tokio::test]
    async fn get_usage_record_plugin_transient_lifts_to_service_unavailable() {
        use usage_collector_sdk::UsageCollectorPluginError;

        // Build a tiny one-shot stub that always returns Transient on
        // get_usage_record; HappyPathPlugin doesn't expose a transient
        // path, so this is a minimal inline plugin whose other SPI
        // methods all fail loudly.
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

// The plural `create_usage_records` path is exercised by the `pdp_dedup`,
// `gts_id_dedup` and `invalidation_target_batch` modules; the singular path
// has its own PDP / declaration / target / metadata / SPI sequencing in
// `service.rs::create_usage_record` — an in-line lookup rather than the
// deduped fan-out, so the two are separate code and need separate cover.
// These tests pin one outcome per stage: PDP deny, plugin-reported
// transient on the persist SPI, a target that resolves to nothing, a target
// that is itself a withdrawal, an unfaithful copy, the copy rule outranking
// the metadata rule, the store's at-most-one rejection, a backend fault on
// the target read, and the happy path.

mod create_usage_record_path_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        ConflictReason, CreateUsageRecord, IdempotencyKey, Invalidation, MetadataKey, MeterTypeId,
        ReasonCode, RecordOrigin, ResourceRef, USAGE_RECORD_RESOURCE, UsageCollectorError,
        UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord, ValidationReason,
    };
    use uuid::Uuid;

    use super::assert_translatable_scope;
    use crate::domain::Service;
    use crate::domain::test_support::{
        DenyAllResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx, enforcer_for,
        fake_declaration_source_with_fold, fake_declaration_source_with_metadata, hub_with_plugin,
        projected, recent_window_end, recent_window_start,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// Build an ordinary measurement (no `invalidates`) with the given
    /// `tenant_id` and `value`. Used as the base shape every test in this
    /// module shapes — call sites mutate `value` / `gts_type_id` /
    /// `invalidation` to drive the per-stage outcome. There is no
    /// caller-visible `kind` to gate a value-sign rule on, so `value` no
    /// longer drives any accept/reject decision here — it is carried only
    /// because `CreateUsageRecord` requires one, and because a withdrawal
    /// has to echo it.
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
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// A withdrawal of `target`: a faithful copy of
    /// `counter_record(tenant_id, 10, ..)`, departing only in its own
    /// idempotency key and the withdrawal itself. The quantity is echoed,
    /// never negated — an invalidation removes a measurement rather than
    /// offsetting it
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    fn counter_withdrawal(tenant_id: Uuid, target: Uuid, idem: &str) -> CreateUsageRecord {
        let mut r = counter_record(tenant_id, 10, idem);
        r.invalidation = Some(Invalidation {
            target,
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        });
        r
    }

    /// The persisted entry [`counter_withdrawal`] copies faithfully: same
    /// caller-supplied fields, its own identity, its own idempotency key
    /// and its own origin.
    fn target_row(tenant_id: Uuid, id: Uuid) -> UsageRecord {
        UsageRecord {
            id,
            idempotency_key: IdempotencyKey::new("idem-target").expect("valid idempotency key"),
            // Deliberately not the origin the live submission will carry: a
            // withdrawal's route is its own, so a mismatch here MUST NOT
            // make the copy unfaithful (`faithful_copy_mismatch` ignores
            // `origin`). This fixture is what pins that.
            origin: RecordOrigin::Backfill,
            ..projected(&counter_record(tenant_id, 10, "idem-target"))
        }
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
            "PDP deny MUST short-circuit before any target lookup",
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

    /// A faithful withdrawal of a resolvable target is accepted, and costs
    /// exactly one target read.
    #[tokio::test]
    async fn create_usage_record_accepts_a_faithful_withdrawal_after_one_target_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x703);
        let target = Uuid::from_u128(0x800);
        plugin.set_get_record(target_row(tenant_id, target));

        let submission = counter_withdrawal(tenant_id, target, "idem-faithful");
        plugin.set_create_record(projected(&submission));

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.faithful.records.v1",
        );

        service
            .create_usage_record(&authenticated_ctx(), submission)
            .await
            .expect("a faithful withdrawal of a resolvable target MUST be accepted");

        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "an invalidation MUST resolve its target exactly once",
        );

        assert_translatable_scope(&plugin, "Service::create_usage_record_inner");
    }

    /// `invalidates` names a uuid the plugin does not hold ⇒ `NotFound`
    /// naming the target. The valid-reference rule of
    /// `cpt-cf-usage-collector-adr-append-only-invalidation`, on the
    /// singular path (the batch path has its own cover in
    /// `invalidation_target_batch_tests`).
    #[tokio::test]
    async fn create_usage_record_unresolvable_target_returns_not_found() {
        let plugin = HappyPathPlugin::new();

        let missing = Uuid::from_u128(0x801);
        plugin.set_get_usage_record_not_found(missing);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.target_not_found.records.v1",
        );

        let record = counter_withdrawal(Uuid::from_u128(0x705), missing, "idem-missing-target");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("an unresolvable target MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, ref detail, .. }
                    if resource_type == USAGE_RECORD_RESOURCE
                        && name == &missing.to_string()
                        && detail.contains(&missing.to_string())
            ),
            "an unresolvable `invalidates` MUST surface as NotFound naming the \
             caller-supplied target; got {err:?}",
        );
        assert!(
            plugin.last_create_record_input().is_none(),
            "an unresolvable target MUST short-circuit before the persist SPI",
        );
    }

    /// `invalidates` names an entry that is itself a withdrawal ⇒
    /// `InvalidationTargetNotRecord`. A correction cannot be reversed: the
    /// entry that withdrew a measurement is not itself withdrawable.
    #[tokio::test]
    async fn create_usage_record_target_that_is_an_invalidation_is_refused() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x707);
        let target = Uuid::from_u128(0x802);

        let mut row = target_row(tenant_id, target);
        row.invalidation = Some(Invalidation {
            target: Uuid::from_u128(0x80F),
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        });
        plugin.set_get_record(row);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.target_not_record.records.v1",
        );

        let err = service
            .create_usage_record(
                &authenticated_ctx(),
                counter_withdrawal(tenant_id, target, "idem-withdraw-a-withdrawal"),
            )
            .await
            .expect_err("withdrawing a withdrawal MUST surface as Err");

        match err {
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationTargetNotRecord,
                field,
                resource_name,
                ..
            } => {
                assert_eq!(field, "invalidates");
                assert_eq!(resource_name.as_deref(), Some(target.to_string().as_str()));
            }
            other => panic!(
                "a target that is itself an invalidation MUST surface as \
                 InvalidationTargetNotRecord; got {other:?}",
            ),
        }
        assert!(
            plugin.last_create_record_input().is_none(),
            "a non-record target MUST short-circuit before the persist SPI",
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
        let target = Uuid::from_u128(0x803);

        let mut row = target_row(tenant_id, target);
        row.value = rust_decimal::Decimal::from(999);
        plugin.set_get_record(row);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.field_mismatch.records.v1",
        );

        let err = service
            .create_usage_record(
                &authenticated_ctx(),
                counter_withdrawal(tenant_id, target, "idem-unfaithful"),
            )
            .await
            .expect_err("an unfaithful copy MUST surface as Err");

        match err {
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationFieldMismatch,
                field,
                detail,
                ..
            } => {
                assert_eq!(
                    field, "value",
                    "the rejection MUST name the field that differs"
                );
                assert!(
                    !detail.contains("999"),
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
        let target = Uuid::from_u128(0x804);

        let mut row = target_row(tenant_id, target);
        row.value = rust_decimal::Decimal::from(999);
        plugin.set_get_record(row);

        let mut submission = counter_withdrawal(tenant_id, target, "idem-both-broken");
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

        let err = service
            .create_usage_record(&authenticated_ctx(), submission)
            .await
            .expect_err("a submission breaking both rules MUST surface as Err");

        match err {
            UsageCollectorError::InvalidArgument { reason, .. } => assert_eq!(
                reason,
                ValidationReason::InvalidationFieldMismatch,
                "the copy rejection MUST outrank the metadata one",
            ),
            other => panic!("expected a validation rejection, got {other:?}"),
        }
    }

    /// The store's at-most-one rule, lifted verbatim. The gateway runs no
    /// pre-read for it — only the store can make the check atomic with the
    /// entry it admits.
    #[tokio::test]
    async fn create_usage_record_plugin_already_invalidated_lifts_to_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70A);
        let target = Uuid::from_u128(0x805);
        let existing = Uuid::from_u128(0x806);
        plugin.set_get_record(target_row(tenant_id, target));
        plugin.set_create_record_err(UsageCollectorPluginError::AlreadyInvalidated {
            id: target,
            invalidated_by: existing,
        });

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.already_invalidated.records.v1",
        );

        let err = service
            .create_usage_record(
                &authenticated_ctx(),
                counter_withdrawal(tenant_id, target, "idem-second-withdrawal"),
            )
            .await
            .expect_err("a second withdrawal MUST surface as Err");

        match err {
            UsageCollectorError::Conflict {
                reason: ConflictReason::AlreadyInvalidated,
                name,
                detail,
                ..
            } => {
                assert_eq!(name, target.to_string());
                assert!(detail.contains(&existing.to_string()));
            }
            other => panic!("at-most-one is the store's rule to report; got {other:?}"),
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

        let err = service
            .create_usage_record(
                &authenticated_ctx(),
                counter_withdrawal(Uuid::from_u128(0x70B), Uuid::from_u128(0x807), "idem-t"),
            )
            .await
            .expect_err("a transient target read MUST surface as Err");

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
    /// refused here too. This path reads one id and gets one row back, so
    /// it has even less excuse than the batch fan-out; the submission is
    /// again a faithful copy of the returned row, so without the check it
    /// is accepted.
    #[tokio::test]
    async fn create_usage_record_refuses_a_plugin_that_answers_with_the_wrong_row() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70C);
        let requested = Uuid::from_u128(0x808);
        let answered = Uuid::from_u128(0x809);
        plugin.set_get_record(target_row(tenant_id, answered));

        // Programmed so a gateway that believed the row would return
        // `Ok(..)` — the false accept — rather than tripping over an
        // unprogrammed SPI and reporting the wrong defect.
        let submission = counter_withdrawal(tenant_id, requested, "idem-wrong-row");
        plugin.set_create_record(projected(&submission));

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.wrong_row.records.v1",
        );

        let err = service
            .create_usage_record(&authenticated_ctx(), submission)
            .await
            .expect_err("a mis-answering plugin MUST surface as Err");

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
            plugin.last_create_record_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
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
            "an entry carrying no `invalidates` MUST NOT trigger a target lookup",
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

    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef, UsageCollectorError,
        UsageCollectorPluginV1, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::service::MAX_BATCH_RECORDS;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, recent_window_end, recent_window_start,
    };

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

// ── Service-level server-assigned stamps (in-process / SDK callers) ────────
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
//
// `origin` is stamped at the same point and from the same call, so it is
// pinned here too: it is server-assigned from the route, and these two
// methods ARE the live route.
#[cfg(test)]
mod server_assigned_stamp_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef,
        UsageCollectorPluginV1, derive_usage_record_id,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected, recent_window_end, recent_window_start,
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
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
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

    /// Both live ingestion surfaces MUST stamp `origin = live` on what they
    /// dispatch.
    ///
    /// Read off the DISPATCHED record rather than off the returned one: the
    /// plugin echoes a fixture back, so a returned `Live` would only prove
    /// the fixture was built `Live`. What the service handed the plugin is
    /// the value it assigned.
    #[tokio::test]
    async fn the_live_surfaces_stamp_origin_live() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD3);
        let single = input_record(tenant_id, "idem-origin-singular");
        let batch = vec![input_record(tenant_id, "idem-origin-batch")];

        plugin.set_create_record(projected(&single));
        plugin.set_create_records(batch.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.origin_stamp.live.records.v1",
        );

        service
            .create_usage_record(&authenticated_ctx(), single)
            .await
            .expect("happy path MUST accept the record");
        assert_eq!(
            plugin
                .last_create_record_input()
                .expect("plugin received the dispatched record")
                .origin,
            RecordOrigin::Live,
            "create_usage_record IS the live route, so the entry it \
             dispatches MUST carry origin = live",
        );

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

    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, MeterTypeId, ResourceRef, UsageCollectorError,
        UsageCollectorPluginV1,
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

// ── The live path's two-sided covered-period bound, at the service ─────────
//
// `covered_period_tests.rs` pins the rule itself. These pin that the
// Ingestion Gateway applies it — on both entry points, before the PDP call
// and before any plugin dispatch — and that it governs an invalidation over
// the period the invalidation copies, which is the case nothing in this
// tree pinned before.
mod covered_period_bounds_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        BACKFILL_ROUTE_PATH, CreateUsageRecord, IdempotencyKey, Invalidation, MeterTypeId,
        ReasonCode, ResourceRef, UsageCollectorError, UsageCollectorPluginV1, UsageRecord,
        ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] wired with an arbitrary working declaration —
    /// these tests exercise the covered-period bound, not the declaration.
    /// The bound is enforced ahead of the resolver, so the declaration only
    /// has to let an admitted entry through.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A submission the live path admits: the shared recent covered period,
    /// an hour long and closed an hour ago.
    fn fresh_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-bounds", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The same submission, differing in the covered period and in nothing
    /// else: a month back, well beyond the 48-hour live past tolerance and
    /// equally well short of the boundary, so the outcome does not depend on
    /// how loaded the runner is.
    ///
    /// Offset from the memoised [`recent_window_end`] rather than from a
    /// fresh `now_utc()`, so two submissions built by separate calls carry
    /// the SAME period. A per-call clock read would land them microseconds
    /// apart, and a withdrawal built that way is not the faithful copy of
    /// its target it claims to be — it would be refused for
    /// `InvalidationFieldMismatch` and the test below would pass on the
    /// wrong rejection.
    fn stale_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        let window_end = recent_window_end() - time::Duration::days(30);
        CreateUsageRecord {
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
        plugin.set_create_record(projected(&submission));
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_past.record.v1",
        );

        let err = service
            .create_usage_record(&authenticated_ctx(), submission)
            .await
            .expect_err("a period 30 days old is beyond the 48-hour live tolerance");

        let (reason, detail) = invalid_argument(&err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(
            detail.contains(BACKFILL_ROUTE_PATH),
            "the rejection MUST name the route the entry belongs on: {detail}",
        );
        assert_eq!(
            plugin.last_create_record_input(),
            None,
            "a refused period MUST NOT reach the storage plugin",
        );
    }

    #[tokio::test]
    async fn the_live_path_refuses_a_period_ending_beyond_the_future_tolerance() {
        // The other side of the bound, and the only test that reaches the
        // future tolerance THROUGH the Service — the unit tests build
        // `CoveredPeriodBounds` by hand, so they never exercise the
        // configured value's route from `[usage_collector]` to this call.
        // Without this, a projection that fed `backfill_window` in as the
        // future tolerance would ship green.
        let plugin = HappyPathPlugin::new();
        // `recent_window_end()` is an hour BEHIND now, so two hours on from
        // it is an hour AHEAD — unambiguously outside a five-minute
        // tolerance and nowhere near the boundary. Anchored on the memoised
        // fixture rather than on a fresh clock read, for the reason
        // `stale_record` documents.
        let window_end = recent_window_end() + time::Duration::hours(2);
        let submission = CreateUsageRecord {
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(Uuid::from_u128(0xC5), "idem-future")
        };
        plugin.set_create_record(projected(&submission));
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_future.record.v1",
        );

        let err = service
            .create_usage_record(&authenticated_ctx(), submission)
            .await
            .expect_err("a period ending an hour from now is beyond the 5-minute tolerance");

        let (reason, detail) = invalid_argument(&err);
        assert_eq!(*reason, ValidationReason::FutureWindow);
        assert!(
            !detail.contains(BACKFILL_ROUTE_PATH),
            "the backfill route lifts the PAST bound only, so pointing a \
             clock-skewed emitter at it would send a defect somewhere it is \
             just as invalid: {detail}",
        );
        assert_eq!(
            plugin.last_create_record_input(),
            None,
            "a refused period MUST NOT reach the storage plugin",
        );
    }

    #[tokio::test]
    async fn the_live_path_refuses_a_withdrawal_over_a_period_older_than_the_past_tolerance() {
        // The failure mode of the whole slice, and the thing no test in the
        // tree pinned before this one. The bound belongs to the path, not to
        // the entry kind: an invalidation is a faithful copy of its target,
        // so its `window_end` IS the target's, and withdrawing a closed
        // month is refused on the live path exactly as a fresh measurement
        // of that month would be
        // (`cpt-cf-usage-collector-adr-backfill-isolation`). `origin =
        // backfill` on a correction is the ordinary case, not a rare one.
        //
        // Getting this backwards is easy and silent. If the bound read the
        // arrival instant instead of the copied period, this submission
        // would be accepted and a correction of closed history would persist
        // reading `origin = live` — the exact gap the past bound closes.
        let tenant_id = Uuid::from_u128(0xC2);
        let target = Uuid::from_u128(0x6C2);

        // The target: a measurement whose period closed a month ago, stored
        // under the identity the gateway would have derived for it.
        let measurement = stale_record(tenant_id, "idem-target");
        let target_row = UsageRecord {
            id: target,
            ..projected(&measurement)
        };
        // The withdrawal: a faithful copy of that measurement, departing
        // only in the two permitted places — its own idempotency key and the
        // reference itself. Its covered period is the target's, which is
        // the whole point.
        let withdrawal = CreateUsageRecord {
            invalidation: Some(Invalidation {
                target,
                reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
            }),
            ..stale_record(tenant_id, "idem-withdrawal")
        };

        let plugin = HappyPathPlugin::new();
        plugin.set_get_record(target_row);
        plugin.set_create_record(projected(&withdrawal));
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_past.withdrawal.v1",
        );

        let err = service
            .create_usage_record(&authenticated_ctx(), withdrawal)
            .await
            .expect_err("a withdrawal of a closed month belongs on the backfill route");

        // The rejection must arrive on the PERIOD, not on the target — so
        // assert the typed reason rather than mere failure, and assert the
        // target was never read. Both still hold if the faithful-copy
        // comparator is later reordered; neither holds if the bound moved
        // behind the target lookup.
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
        // ADR confirmation case 2, and the one place it can be pinned: the
        // bound reads the END of the covered period, so a monthly accrual
        // meter emitting the moment its month closes is admitted even
        // though the period it covers is 30 days long — fifteen times the
        // live past tolerance.
        //
        // The rule itself cannot express the mistake (the function is
        // handed one instant), but this call site can: hand it
        // `record.window_start` instead of `record.window_end` and this
        // test is the only one that goes red, because every other ingestion
        // fixture in the tree covers an hour and the swap is invisible
        // inside a tolerance measured in days.
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xC4);
        let window_end = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
        let submission = CreateUsageRecord {
            window_start: window_end - time::Duration::days(30),
            window_end,
            ..fresh_record(tenant_id, "idem-month-long")
        };
        plugin.set_create_record(projected(&submission));
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_past.month_long.v1",
        );

        service
            .create_usage_record(&authenticated_ctx(), submission)
            .await
            .expect("a 30-day period that closed a minute ago is live consumption");
    }

    #[tokio::test]
    async fn a_batch_rejects_only_the_entries_whose_period_is_out_of_bounds() {
        // Per-submission, at its own input index, with the surviving entries
        // still dispatched — the same posture the projection's own period
        // preconditions already have. A batch-level rejection here would
        // make one stale entry discard a whole import.
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
// `Service::backfill_usage_records` is the live batch body under a
// different `origin`, so what is worth pinning here is only what the origin
// changes: the marker every accepted entry carries, the past bound it
// lifts, that the lift reaches a withdrawal of closed history, and the PDP
// verb it derives per entry from the covered period.
//
// The ADR's Confirmation section is the list
// (`cpt-cf-usage-collector-adr-backfill-isolation`). Its first two cases
// are the live path's and live in `covered_period_bounds_tests` above; its
// last is a concurrent load test against the workload-isolation NFR, which
// is unimplemented and out of scope here — see the TODO at
// `Service::backfill_usage_records`.
mod backfill_route_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use authz_resolver_sdk::AuthZResolverApi;
    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        BACKFILL_ROUTE_PATH, CreateUsageRecord, IdempotencyKey, Invalidation, MeterTypeId,
        ReasonCode, RecordOrigin, ResourceRef, UsageCollectorError, UsageCollectorPluginV1,
        UsageRecord, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        ActionRecordingPermitResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx,
        default_covered_period_bounds, fake_declaration_source_with_fold, projected,
        projected_with_origin, recent_window_end, recent_window_start,
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
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The same submission over a covered period `days` old, and differing
    /// in nothing else.
    ///
    /// Offset from the memoised [`recent_window_end`] rather than from a
    /// fresh `now_utc()`, so two submissions built by separate calls carry
    /// the SAME period — two clock reads land microseconds apart, which
    /// derives two different ids and makes a withdrawal built that way an
    /// unfaithful copy of its target.
    fn aged_record(tenant_id: Uuid, resource_id: &str, idem: &str, days: i64) -> CreateUsageRecord {
        let window_end = recent_window_end() - time::Duration::days(days);
        CreateUsageRecord {
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

    /// ADR confirmation case 5, the write half — `api/rest/dto_tests.rs`
    /// carries the read half.
    ///
    /// Two entries, and the fresh one is the load-bearing half: a batch of
    /// nothing but aged periods would be stamped correctly by a route that
    /// derived `origin` from how old the period is rather than from the
    /// entry point, and the marker's whole job is to record the path the
    /// entry travelled. The assertion is on what the gateway HANDED the
    /// plugin, not on what the plugin handed back — the echo is a fixture
    /// and would answer `backfill` however the entry was stamped.
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

    /// ADR confirmation case 3, both halves in one test so the two can
    /// never drift into agreeing with each other by accident: the live path
    /// refuses the period and names the route, and the route admits it.
    #[tokio::test]
    async fn the_backfill_route_admits_the_period_the_live_path_rejected() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD2);
        let stale = aged_record(tenant_id, "rsc-both", "idem-both", 30);
        // Armed to succeed on BOTH surfaces, so neither outcome below can
        // come from an unprogrammed plugin.
        plugin.set_create_record(projected(&stale));
        plugin.set_create_records(vec![Ok(projected_with_origin(
            &stale,
            RecordOrigin::Backfill,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.backfill.admits.records.v1",
        );

        let live_err = service
            .create_usage_record(&authenticated_ctx(), stale.clone())
            .await
            .expect_err("30 days is beyond the 48-hour live past tolerance");
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

    /// The other side of the bound the route lifts, and the reason DESIGN
    /// §3.3 says the route lifts the past bound "and nothing else".
    ///
    /// The wiring is true by construction — both batch wrappers share
    /// `create_usage_records_for_origin`, and
    /// `enforce_covered_period_bounds` nests only the PAST comparison
    /// inside `origin == Live` — but "true by construction" is what the
    /// `origin` metric label was before a mutation showed nothing pinned
    /// it. A future-dated entry is a clock-skewed emitter, and pointing one
    /// at the import route would send a defect somewhere it is just as
    /// invalid.
    ///
    /// The rejection is per-record, at the entry's input index, because a
    /// refused covered period is a precondition of that submission's
    /// identity derivation rather than of the batch.
    #[tokio::test]
    async fn the_backfill_route_refuses_a_period_ending_beyond_the_future_tolerance() {
        assert!(
            time::Duration::hours(1) > default_covered_period_bounds().future_tolerance,
            "the fixture puts the period an hour ahead of now and needs that \
             to be outside the configured tolerance, which is {:?}",
            default_covered_period_bounds().future_tolerance,
        );

        let plugin = HappyPathPlugin::new();
        // `recent_window_end()` is an hour BEHIND now, so two hours on from
        // it is an hour AHEAD. Anchored on the memoised fixture rather than
        // on a fresh clock read, for the reason `aged_record` documents.
        let window_end = recent_window_end() + time::Duration::hours(2);
        let submission = CreateUsageRecord {
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(Uuid::from_u128(0xD5), "rsc-future", "idem-import-future")
        };
        // Armed to succeed, so the rejection below can only come from the
        // bound and not from an unprogrammed plugin.
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

    /// ADR confirmation case 4.
    ///
    /// The target is built with `origin = Live` and withdrawn on the
    /// backfill route. `origin` records the path each entry travelled and
    /// is NOT one of the fields the faithful-copy rule compares, so a
    /// `live` target and a `backfill` withdrawal is the ordinary pair
    /// rather than a mismatch — a correction found weeks later is exactly
    /// how history gets corrected. If the comparator ever started reading
    /// `origin`, this is the test that goes red.
    #[tokio::test]
    async fn a_withdrawal_of_closed_history_is_refused_live_and_accepted_on_backfill() {
        let tenant_id = Uuid::from_u128(0xD3);
        let target = Uuid::from_u128(0x6D3);

        // The measurement whose period closed a month ago, persisted as the
        // live path would have left it.
        let measurement = aged_record(tenant_id, "rsc-withdraw", "idem-target", 30);
        let target_row = UsageRecord {
            id: target,
            ..projected_with_origin(&measurement, RecordOrigin::Live)
        };
        assert_eq!(
            target_row.origin,
            RecordOrigin::Live,
            "the target must be a LIVE entry, or the pair this test is about \
             is not the pair it built",
        );
        // A faithful copy of it, departing only in the two permitted
        // places — its own idempotency key, and the reference itself.
        let withdrawal = CreateUsageRecord {
            invalidation: Some(Invalidation {
                target,
                reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
            }),
            ..aged_record(tenant_id, "rsc-withdraw", "idem-withdrawal", 30)
        };

        let plugin = HappyPathPlugin::new();
        plugin.set_get_record(target_row.clone());
        plugin.set_create_record(projected(&withdrawal));
        plugin.set_create_records(vec![Ok(projected_with_origin(
            &withdrawal,
            RecordOrigin::Backfill,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.backfill.withdrawal.records.v1",
        );

        let live_err = service
            .create_usage_record(&authenticated_ctx(), withdrawal.clone())
            .await
            .expect_err("a withdrawal of a closed month belongs on the backfill route");
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
    /// hash/eq precisely so a batch bound to different actions cannot
    /// collapse onto a single PDP decision. The backfill route is the first
    /// caller that mixes them; before this test the property was structural
    /// but unexercised.
    ///
    /// Both entries share ONE attribution tuple — same tenant, same
    /// resource, no subject — so the only thing keeping them apart is the
    /// action. Drop `action` from the key and the two collapse onto one
    /// decision, and the entry reaching past the backfill window rides in
    /// on the other's `create` permit.
    ///
    /// The assertion is on the recorded action STRINGS. Counting calls
    /// would pass for the wrong reason twice over: two calls is also what a
    /// dedup broken on some other field produces, and one call is what the
    /// correct implementation produces if the fixture's two periods stopped
    /// straddling the window — which is why the straddle is asserted below
    /// rather than left to the reader to recompute from two literals.
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
// it left the aggregate path (see the comment above
// `aggregate_declared_fold_tests`): validation runs against the meter's
// resolved declaration instead of a plugin-owned catalog row, and the
// per-distinct-`gts_id` catalog fan-out in `create_usage_records` is now a
// resolver fan-out over the same Type Resolver the aggregate path uses.
mod ingestion_declared_type_tests {
    use std::collections::BTreeMap;

    use std::sync::Arc;

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
        fake_declaration_source_with_metadata, projected, recent_window_end, recent_window_start,
        recording_plugin_resolver,
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

// `CompiledMetadataSchema::declared_keys()` gates the read paths'
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
    /// no time predicate". Before the range became a typed parameter the
    /// second was a 400 too, because the retired guard read the window out
    /// of this same slot.
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
    use usage_collector_sdk::{MeterTypeId, UsageCollectorError, UsageCollectorPluginV1};

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
        MetadataFilter, MeterTypeId, RECORD_ID_FIELD, TimeRange, UsageCollectorError,
        UsageCollectorPluginV1, WINDOW_END_FIELD,
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

    /// The 400 blames the token rather than an `$orderby` the caller cannot
    /// send alongside a cursor, but the gear spells neither that field nor
    /// the wire code (Spec §3.13): both come from the `toolkit_odata` error
    /// the refusal carries. So this pins the carried error, and
    /// `infra::sdk_error_mapping` pins the projection onto the wire.
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
        // The whole reason the range is in the fingerprint. Same caller,
        // same `$filter`, a different range — which used to be a `$filter`
        // conjunct and so was covered by the hash for free.
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
        // The truncation hole `TimeRange::canonical_form` exists to avoid:
        // if the range rendering reused the identity derivation's fixed
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
        // `$filter` was already covered before the range joined it, and an
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
    async fn an_in_process_order_diverging_from_its_token_never_reaches_the_plugin() {
        // The other half of `validate_cursor_against`, and the half that
        // never moved: the order the plugin sorts by must be the order the
        // token's boundary values were minted under. The token here says
        // `(window_end, id)` while the caller's `query.order` says
        // `(id, window_end)` — both are sound keysets naming both canonical
        // fields, so `require_continuation_keyset` passes them, and the
        // fingerprint matches because neither the meter, the range, the
        // filter nor the metadata changed.
        //
        // Left uncaught, the plugin builds its row-value tuple predicate
        // over `(id, window_end)` and compares it against boundary values
        // ordered `(window_end, id)`: a misaligned continuation served as
        // an `Ok`, which is exactly the failure the fingerprint was moved
        // behind the service to prevent. No REST test can reach it — the
        // handler overwrites `query.order` from the token — so the in-process
        // caller is the only one who can express the divergence.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        // Three ways a caller order can disagree with its token, all of
        // them individually admissible so no structural check sees them:
        // permuted, narrower, and wider-with-a-different-lead.
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
            spy.set_list_usage_records_response(ODataPage::empty(0));
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
        // A shape only the binding can catch, and one that was silently
        // admissible in process before it: the token's `s` is malformed, so
        // no order can be derived from it — but the caller supplied a
        // perfectly sound `order` of their own, which the structural check
        // was reading instead. The handler rejects this at the edge, so
        // again only an in-process caller can express it.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let mut malformed = continuation_of(&caller, &fingerprint);
        malformed.cursor.as_mut().expect("cursor").s = String::new();

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(ODataPage::empty(0));
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &malformed, &[])
            .await
            .expect_err("a token whose signed keys do not decode must be refused");

        match err {
            // A malformed token blames the token, not an `$orderby` the
            // caller cannot send alongside a cursor — an attribution
            // `toolkit_odata` supplies, from the error carried here.
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
        // Why the binding runs BEFORE the structural check rather than
        // after. The token is bound to `+resource_id` alone, which names
        // neither canonical field and is not a keyset; the caller supplies
        // the canonical order beside it. Check-then-bind would validate
        // the caller's sound order and then overwrite it with the token's
        // unsound one, handing the plugin exactly the order the check
        // exists to refuse.
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
        spy.set_list_usage_records_response(ODataPage::empty(0));
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
        // The check order is a documented fact (`require_continuation_keyset`
        // says structure is checked before relevance) and it is
        // caller-visible: a token that is BOTH bound to an unsound keyset
        // and minted over another query carries `toolkit_odata`'s
        // `InvalidCursor` today and would carry `FilterMismatch` if the two
        // checks were swapped — a different reason code on the wire, since
        // that is the value the code is derived from, and a different
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

// ── Withdrawal exclusion from the fold ─────────────────────────────────────
//
// The gear folds nothing: `query_aggregated_usage_records` dispatches to the
// plugin and returns what it computes. So these tests do not — and cannot —
// bind a real storage plugin to anything. What they pin is the *reference*
// semantics the SPI now states normatively — the ones the
// `invalidation-excluded-from-fold` contract test DESIGN §3.3 requires of
// every conforming plugin, a suite this repo does not carry — held here
// against the in-memory `FoldingPlugin`. They also catch a gear-side
// regression that changed what the plugin is handed: a dropped
// `time_range`, or a fold other than the declared one, moves these numbers.
mod withdrawal_exclusion_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use bigdecimal::BigDecimal;
    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationFold, AggregationResult, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef,
        UsageCollectorPluginV1, UsageRecord, derive_usage_record_id,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        FOLDING_PLUGIN_SUFFIX, FoldingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_fold, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// The tenant every entry here is attributed to — the one
    /// [`recording_plugin_resolver`] returns as its fixed
    /// `OWNER_TENANT_ID` constraint, so the composed scope names the rows
    /// the fixture holds rather than some other tenant's.
    fn tenant_id() -> Uuid {
        Uuid::from_u128(2)
    }

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A persisted measurement inside [`test_time_range`], carrying `value`
    /// and ending `minutes` after the epoch.
    ///
    /// Built through [`derive_usage_record_id`] rather than a random `id`
    /// so the identity is the one the gateway would have derived, which is
    /// what an invalidation's reference has to name.
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
            ),
            gts_type_id: meter_id(),
            tenant_id: tenant_id(),
            resource_ref: ResourceRef::new("rsc-fold", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: value.parse().expect("valid decimal quantity"),
            idempotency_key,
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
        // The exclusion is load-bearing for correctness, not tidiness: a
        // fold that admitted the echoed quantity would double-count the
        // very measurement the withdrawal was meant to remove
        // (`cpt-cf-usage-collector-adr-append-only-invalidation`).
        //
        // Both entries carry one covered period, so no range selects one of
        // the pair without the other — which is why no placement of the
        // invalidation changes the result.
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Sum);
        let withdrawn = measurement("idem-withdrawn", "42.5", 30);
        plugin.store(measurement("idem-survivor", "7.5", 10));
        let invalidation = plugin.store_withdrawn(withdrawn.clone());

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("7.5")),
            "only the entry nothing withdrew may reach the fold",
        );

        // …and the ledger paths return all three, as persisted. This is a
        // ledger, not a derived view: leaving a withdrawn pair out of a
        // locally computed fold is the reader's obligation, and the
        // reference the invalidation carries is what they read to do it.
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
        // The failure mode the rule exists to prevent, pinned as its own
        // case: a plugin that dropped the withdrawn record but kept the
        // echoed invalidation reports the measurement it was told to
        // remove. Over the pair alone the wrong answer under `SUM` is
        // 42.5 and the right one is an empty selection — so this fails
        // loudly rather than by a rounding.
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
        // The invalidation half of the rule stands on its own: an
        // invalidation contributes nothing to a fold whether or not its
        // target is in the selection. Retention is plugin-owned (DESIGN
        // §3.10), so a conforming deployment can purge a target and keep
        // the entry that withdrew it — this is a reachable state, not a
        // malformed ledger.
        //
        // It is also the one input shape that tells the two half-rules
        // apart. Over a conforming pair the invalidation is a faithful
        // copy, so "kept the target" and "kept the invalidation" give a
        // fold the same number; here only the second admits 42.5.
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
        // The ADR's Confirmation asks for this across all five: withdrawal
        // is the primitive precisely because its meaning does not depend on
        // what a quantity means, and `MAX` / `MIN` / `LATEST` reverse under
        // no additional term at all.
        //
        // Two pairs are withdrawn, not one, and their quantities straddle
        // the survivors' — 99 above and 1 below. With a single withdrawn
        // quantity, either `MAX` or `MIN` would answer the same whether the
        // pair leaked in or not, and that half of the case would pass
        // vacuously. Both pairs also end later than either survivor, so
        // `LATEST` moves too. Admitting the pairs gives 227.5 / 6 / 99 / 1
        // / 1 against the five expected below: every fold moves.
        //
        // All five are collected before a single assertion, so a defect
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
}
