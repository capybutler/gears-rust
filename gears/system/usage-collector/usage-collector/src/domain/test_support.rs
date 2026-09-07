//! Shared test infrastructure for domain-layer unit tests.
//!
//! GTS registry: use `MockTypesRegistryClient` and `make_test_instance` from
//! `types_registry_sdk::testing` directly (gated on the `test-util`
//! dev-dependency feature).
//!
//! PDP: this module exposes `AuthZResolverApi` fakes plus `PolicyEnforcer`
//! constructors that let tests pin every outcome (permit + constraints, deny,
//! empty-constraints fail-closed, transport unreachable, and no-cache via the
//! per-call counting resolvers).
//!
//! Every public helper in this module is test-only — `.lock().expect("…")`
//! on the internal mutexes is fine because a poisoned mutex inside a unit
//! test indicates an unrecoverable test failure anyway. The
//! `missing_panics_doc` lint would force boilerplate `# Panics` sections
//! on every setter/getter; suppressing it module-wide keeps the helpers
//! readable without diluting the production-code expectation.
#![allow(clippy::missing_panics_doc)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use authz_resolver_sdk::constraints::Constraint;
use authz_resolver_sdk::models::{
    DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use toolkit_security::{PlatformSecurityContext, pep_properties};
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, CreateUsageRecord, MetadataFilter,
    MeterTypeId, TimeRange, UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord,
};
use uuid::Uuid;

/// Projects a create submission into its persisted shape, for a plugin echo
/// fixture that must agree with the service on the derived `id`.
///
/// Routing through the real
/// [`CreateUsageRecord::try_into_usage_record`] rather than hand-building a
/// [`UsageRecord`] is the point: a stub that invented its own `id` would
/// pass even if the service stopped deriving one. Every fixture in these
/// tests supplies a valid covered period, so the projection cannot fail —
/// and if one ever does, the `expect` names the fixture as the defect
/// rather than letting a bogus record reach the assertion.
pub(crate) fn projected(submission: &CreateUsageRecord) -> UsageRecord {
    submission
        .clone()
        .try_into_usage_record()
        .expect("test fixture supplies a valid covered period")
}

/// The mandatory read-path range the domain read-path tests hand to
/// `list_usage_records` / `query_aggregated_usage_records`.
///
/// One hour from the epoch — narrow enough that a throwaway "all time"
/// range substituted anywhere on the way to the SPI is not equal to it.
#[must_use]
pub(crate) fn test_time_range() -> TimeRange {
    TimeRange::new(
        time::OffsetDateTime::UNIX_EPOCH,
        time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    )
    .expect("a one-hour range from the epoch is a valid TimeRange")
}

/// Minimal mock storage-plugin client.
///
/// The `UsageCollectorPluginV1` SPI surface carries six methods. The mock
/// here exists purely so the Plugin Host can resolve a concrete
/// `Arc<dyn UsageCollectorPluginV1>` from `ClientHub` and so cache tests can
/// assert `Arc::ptr_eq` on the resolved handle. Every method returns a
/// deterministic `Internal("test_fake: …")` so any test that accidentally
/// dispatches through the mock surfaces an obvious failure; downstream
/// features reshape the mock surface when test bodies need richer fakes.
pub struct MockPlugin;

impl MockPlugin {
    /// Returns a fresh mock plugin as a scoped trait object.
    #[must_use]
    pub fn arc() -> Arc<dyn UsageCollectorPluginV1> {
        Arc::new(Self)
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for MockPlugin {
    async fn create_usage_record(
        &self,
        _record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::create_usage_record not implemented",
        ))
    }

    async fn create_usage_records(
        &self,
        _records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::create_usage_records not implemented",
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
            "test_fake: MockPlugin::query_aggregated_usage_records not implemented",
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
            "test_fake: MockPlugin::list_usage_records not implemented",
        ))
    }

    async fn deactivate_usage_record(&self, _id: Uuid) -> Result<(), UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::deactivate_usage_record not implemented",
        ))
    }

    async fn get_usage_record(
        &self,
        _id: Uuid,
        _scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::get_usage_record not implemented",
        ))
    }
}

// ── PDP (authz-resolver) mocks ─────────────────────────────────────────────

/// Build a permit `EvaluationResponse` carrying a single
/// `property = value` string-equality constraint. Compiles (against a resource
/// type that lists `property` as supported) to a non-empty `AccessScope`, so it
/// exercises the permit-with-constraints `Ok` path through
/// `require_constraints(true)`.
#[must_use]
pub fn permit_with_string_constraint(property: &'static str, value: String) -> EvaluationResponse {
    use authz_resolver_sdk::constraints::{EqPredicate, Predicate};

    EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate::new(property, value))],
            }],
            deny_reason: None,
        },
    }
}

/// Build a permit `EvaluationResponse` scoped to the request's own
/// `OWNER_TENANT_ID`, simulating a tenant-scoped grant that authorizes
/// exactly the tenant the record under test names.
///
/// The per-record (`usage_record`) authz path runs under
/// `require_constraints(true)` and applies an attribution gate
/// (`authz::scope_admits_attribution_tuple`). A plain empty-constraints permit (e.g.
/// [`CountingAllowAllResolver`]) fails closed as
/// `EnforcerError::CompileFailed` there, and a fixed-tenant constraint (e.g.
/// [`permit_with_string_constraint`]) would only satisfy the gate for one
/// hard-coded tenant. Echoing the request's `OWNER_TENANT_ID` back as the
/// granted scope compiles to a non-empty `AccessScope` AND satisfies the gate
/// for ANY record tenant a test picks. When the request carries no
/// `OWNER_TENANT_ID` (the subject-only catalog surface, which runs under
/// `require_constraints(false)`), it falls back to an empty-constraints
/// `allow_all` permit — the legitimate happy-path there.
#[must_use]
pub fn permit_scoped_to_request_tenant(request: &EvaluationRequest) -> EvaluationResponse {
    match request
        .resource
        .properties
        .get(pep_properties::OWNER_TENANT_ID)
        .and_then(serde_json::Value::as_str)
    {
        Some(tenant) => {
            permit_with_string_constraint(pep_properties::OWNER_TENANT_ID, tenant.to_owned())
        }
        None => EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        },
    }
}

/// PDP fake that denies every request whose composed `resource_id` equals
/// `deny_resource_id` and permits every other, scoping each permit to the
/// request's own tenant (see [`permit_scoped_to_request_tenant`]).
///
/// The PEP composer at `domain/authz.rs` populates the request's resource
/// properties with `PROP_RESOURCE_ID` from the attribution tuple, so this
/// resolver discriminates **per attribution tuple** — which is what lets a
/// batch test mix permitted and denied records in one call and assert the
/// per-index outcomes.
#[derive(Debug)]
pub struct DenyOneResourceResolver {
    deny_resource_id: String,
}

impl DenyOneResourceResolver {
    /// Denies the attribution tuple whose `resource_id` is
    /// `deny_resource_id`; permits every other.
    #[must_use]
    pub fn new(deny_resource_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            deny_resource_id: deny_resource_id.into(),
        })
    }
}

#[async_trait]
impl AuthZResolverApi for DenyOneResourceResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let matches_deny = request
            .resource
            .properties
            .get(crate::domain::authz::usage_record::PROP_RESOURCE_ID)
            .and_then(serde_json::Value::as_str)
            == Some(self.deny_resource_id.as_str());
        if matches_deny {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    constraints: Vec::new(),
                    deny_reason: Some(DenyReason {
                        error_code: "test-deny".to_owned(),
                        details: None,
                    }),
                },
            });
        }
        // Permit: scope the grant to the record's own tenant so the
        // per-record gate (`require_constraints(true)`) admits it, rather
        // than an empty-constraints permit that would fail closed as
        // `CompileFailed`.
        Ok(permit_scoped_to_request_tenant(&request))
    }
}

/// Counting PDP fake that permits and scopes the grant to the request's own
/// `OWNER_TENANT_ID` (see [`permit_scoped_to_request_tenant`]), recording the
/// call count so per-record dedup tests can still assert the exact number of
/// PDP round-trips. Use this for the per-record (`usage_record`) paths under
/// `require_constraints(true)` where [`CountingAllowAllResolver`] would fail
/// closed.
#[derive(Debug, Default)]
pub struct CountingTenantPermitResolver {
    calls: AtomicUsize,
}

impl CountingTenantPermitResolver {
    /// Build a tenant-scoped permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingTenantPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(permit_scoped_to_request_tenant(&request))
    }
}

/// Counting PDP fake that always permits with a fixed string-equality
/// constraint and records how many times it was called, so the no-cache test
/// can assert that two identical authorize calls each hit the resolver.
#[derive(Debug)]
pub struct CountingPermitResolver {
    property: &'static str,
    value: String,
    calls: AtomicUsize,
}

impl CountingPermitResolver {
    /// Build a resolver that permits with a single `property = value` string
    /// constraint.
    #[must_use]
    pub fn new(property: &'static str, value: String) -> Arc<Self> {
        Arc::new(Self {
            property,
            value,
            calls: AtomicUsize::new(0),
        })
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(permit_with_string_constraint(
            self.property,
            self.value.clone(),
        ))
    }
}

/// Counting PDP fake that always permits with NO constraints (an
/// `allow_all` decision) and records call counts. Use this whenever the
/// resource type under test declares no supported PEP attributes — a
/// constraint-bearing permit (e.g. [`CountingPermitResolver`]) would fail
/// to compile under such a resource type, surfacing as
/// `EnforcerError::CompileFailed`.
#[derive(Debug, Default)]
pub struct CountingAllowAllResolver {
    calls: AtomicUsize,
}

impl CountingAllowAllResolver {
    /// Build an `allow_all` permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingAllowAllResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that captures the most-recent [`EvaluationRequest`] it received
/// (so tests can inspect the request shape the PEP composed) AND scopes its
/// permit to the request's own `OWNER_TENANT_ID` (see
/// [`permit_scoped_to_request_tenant`]). Used by the `authz_tests`
/// equivalence regression to prove the per-record and per-tuple PDP composers
/// emit byte-identical requests for the same input; the tenant-scoped permit
/// is what lets those calls return `Ok` under the per-record gate's
/// `require_constraints(true)` posture (a plain empty-constraints permit would
/// fail closed there).
#[derive(Debug, Default)]
pub struct CapturingTenantPermitResolver {
    last_request: std::sync::Mutex<Option<EvaluationRequest>>,
}

impl CapturingTenantPermitResolver {
    /// Build a capturing tenant-scoped permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns and clears the most-recently captured request.
    #[must_use]
    pub fn take_last_request(&self) -> Option<EvaluationRequest> {
        self.last_request.lock().expect("mutex").take()
    }
}

#[async_trait]
impl AuthZResolverApi for CapturingTenantPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let response = permit_scoped_to_request_tenant(&request);
        *self.last_request.lock().expect("mutex") = Some(request);
        Ok(response)
    }
}

/// PDP fake that permits but returns an EMPTY constraint set. With
/// `require_constraints(true)` the PEP fails this closed as
/// `EnforcerError::CompileFailed` (`empty_constraints`).
#[derive(Debug, Default)]
pub struct PermitEmptyConstraintsResolver;

#[async_trait]
impl AuthZResolverApi for PermitEmptyConstraintsResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that denies every evaluation (`decision: false`), surfacing as
/// `EnforcerError::Denied` (`deny`).
#[derive(Debug, Default)]
pub struct DenyAllResolver;

#[async_trait]
impl AuthZResolverApi for DenyAllResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake whose transport is unreachable: every evaluation returns
/// `AuthZResolverError::ServiceUnavailable`, surfacing as
/// `EnforcerError::EvaluationFailed` (`unreachable`).
#[derive(Debug, Default)]
pub struct UnreachableResolver;

#[async_trait]
impl AuthZResolverApi for UnreachableResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::service_unavailable()
            .with_detail(
                "usage-collector test fake: simulated authz-resolver transport failure".to_owned(),
            )
            .create())
    }
}

/// PDP fake that combines an unreachable transport with a call counter,
/// so handler tests asserting a pre-service short-circuit can pin
/// `calls() == 0` as direct evidence the service path was never reached.
#[derive(Debug, Default)]
pub struct CountingUnreachableResolver {
    calls: AtomicUsize,
}

impl CountingUnreachableResolver {
    /// Build a counting unreachable resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingUnreachableResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(CanonicalError::service_unavailable()
            .with_detail(
                "usage-collector test fake: simulated authz-resolver transport failure".to_owned(),
            )
            .create())
    }
}

/// Wrap a `dyn AuthZResolverApi` in a `PolicyEnforcer` for tests, mirroring
/// the production `module.rs` wiring (`PolicyEnforcer::new(authz)`). No
/// capabilities are advertised, matching production (`module.rs:74`):
/// `usage_record` is a flat resource that does not advertise
/// `Capability::TenantHierarchy`, so the PDP expands a caller's tenant closure
/// eagerly into a flat `OWNER_TENANT_ID In [..]` constraint rather than pushing
/// it down as an `InTenantSubtree` predicate (see
/// [`crate::domain::authz::authorize_list_usage_records`]). A test that needs a
/// hierarchy-aware enforcer must build one explicitly and assert the
/// `InTenantSubtree` fail-closed behavior at that call site.
#[must_use]
pub fn enforcer_for(authz: Arc<dyn AuthZResolverApi>) -> PolicyEnforcer {
    PolicyEnforcer::new(authz)
}

// ── Plugin Host wiring helpers (`ClientHub` + scoped plugin) ────────────────

use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit_security::SecurityContext;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use usage_collector_sdk::UsageCollectorPluginSpecV1;

use crate::domain::Service;
use crate::domain::ports::declarations::UnavailableDeclarationSource;
use crate::domain::type_resolver::{TypeResolver, TypeResolverConfig};

/// A Type Resolver with no working backend, for `Service` test builders in
/// this module that don't exercise type resolution at all (the plugin-host /
/// PDP / metrics paths). Mirrors `Service::new`'s own default — see
/// [`UnavailableDeclarationSource`] for why a domain-only placeholder is used
/// here rather than the real `types-registry` adapter (that would require
/// this domain-layer module to import a concrete `infra` type).
#[must_use]
fn inert_type_resolver(metrics: Arc<dyn UsageCollectorMetrics>) -> Arc<TypeResolver> {
    Arc::new(TypeResolver::new(
        Arc::new(UnavailableDeclarationSource),
        TypeResolverConfig {
            ttl: std::time::Duration::from_secs(1),
            capacity: 1,
        },
        metrics,
    ))
}

/// Build a usage-collector storage-plugin instance id under the schema
/// prefix advertised by [`UsageCollectorPluginSpecV1`], with `suffix` as
/// the five-token instance tail (e.g. `"test.happy_path.records.v1"`).
#[must_use]
pub fn usage_collector_instance_id(suffix: &str) -> String {
    format!("{}{suffix}", UsageCollectorPluginSpecV1::gts_type_id())
}

fn plugin_instance_content(gts_id: &str, vendor: &str) -> serde_json::Value {
    serde_json::json!({
        "id": gts_id,
        "vendor": vendor,
        "priority": 0,
        "properties": {}
    })
}

/// Wire a fresh [`ClientHub`] with a `MockTypesRegistryClient` advertising
/// one usage-collector plugin instance and a scoped client binding the
/// supplied `plugin` under that instance id.
#[must_use]
pub fn hub_with_plugin(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    suffix: &str,
    vendor: &str,
) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    let instance_id = usage_collector_instance_id(suffix);
    let instance = make_test_instance(&instance_id, plugin_instance_content(&instance_id, vendor));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);
    hub.register_scoped::<dyn UsageCollectorPluginV1>(ClientScope::gts_id(&instance_id), plugin);
    hub
}

/// Fixture parameters for building a test [`Service`] against a
/// caller-supplied plugin stub (registered under the `cyberfabric` vendor).
///
/// Replaces the eight-function `service_with_*` suffix family
/// (`service_with_permit`, `_counting_permit`, `_permit_and_source`,
/// `_counting_permit_and_source`, `_metrics`, `_metrics_and_source`,
/// `_recording_plugin`, `_recording_plugin_and_cap`) that grew from three
/// independent axes — whether the Type Resolver works over a real
/// [`DeclarationSource`], which PDP fake backs the enforcer, and whether the
/// metrics sink is a real adapter — each of which used to mint a new
/// function name per combination (the next axis would have produced
/// `_and_source_and_cap`). Each axis is now a setter; a future axis is a new
/// method, not a ninth function. Every terminal method funnels through the
/// single private [`build_service`] constructor.
///
/// Defaults: the inert [`UnavailableDeclarationSource`] Type Resolver (see
/// [`inert_type_resolver`]), [`crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES`],
/// and an internally-built [`CountingTenantPermitResolver`] PDP fake (scopes
/// its permit to the request's own `OWNER_TENANT_ID`, so per-record paths
/// under `require_constraints(true)` pass the tenant gate for whatever
/// tenant the record names, while the subject-only catalog surface —  no
/// `OWNER_TENANT_ID`, `require_constraints(false)` — still gets an
/// `allow_all` permit).
#[derive(Default)]
pub(crate) struct ServiceFixture {
    source: Option<Arc<dyn DeclarationSource>>,
    cap: Option<usize>,
    resolver: Option<Arc<dyn AuthZResolverApi>>,
}

impl ServiceFixture {
    /// Wire a working Type Resolver over `source` instead of the inert
    /// default — for tests that must reach declaration resolution
    /// (`create_usage_record{,s}` and `query_aggregated_usage_records` both
    /// resolve the referenced meter's declaration before dispatch).
    #[must_use]
    pub(crate) fn with_source(mut self, source: Arc<dyn DeclarationSource>) -> Self {
        self.source = Some(source);
        self
    }

    /// Configure a non-default `metadata_size_cap_bytes`, for tests pinning
    /// that a non-default configured cap is actually honoured by the
    /// ingestion path.
    #[must_use]
    pub(crate) fn with_cap(mut self, cap: usize) -> Self {
        self.cap = Some(cap);
        self
    }

    /// Use `resolver` as the PDP fake instead of the default
    /// tenant-scoped [`CountingTenantPermitResolver`] — for tests that need
    /// a specific decision shape (deny, unreachable, a fixed constraint, …).
    #[must_use]
    pub(crate) fn with_resolver(mut self, resolver: Arc<dyn AuthZResolverApi>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Build the `Service` against a `NoopMetrics` sink, discarding the PDP
    /// fake handle.
    #[must_use]
    pub(crate) fn build(
        self,
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> Arc<Service> {
        build_service(self, plugin, suffix, Arc::new(NoopMetrics)).0
    }

    /// Build the `Service` against a `NoopMetrics` sink, exposing the
    /// default [`CountingTenantPermitResolver`] fake so the test can assert
    /// the exact number of PDP `evaluate` round-trips the service issued.
    ///
    /// Named for what it hands back, not just how it builds: this is
    /// `.build()` plus the default resolver's handle, and is mutually
    /// exclusive with `.with_resolver(..)` — there would be no
    /// `CountingTenantPermitResolver` handle to return for a caller-supplied
    /// fake, so pass one here specifically when the default hasn't been
    /// overridden.
    ///
    /// # Panics
    ///
    /// Panics (test-only) if `.with_resolver` overrode the default fake —
    /// there would be no `CountingTenantPermitResolver` handle to hand back.
    #[must_use]
    pub(crate) fn build_with_default_resolver_handle(
        self,
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> (Arc<Service>, Arc<CountingTenantPermitResolver>) {
        assert!(
            self.resolver.is_none(),
            "build_with_default_resolver_handle expects the default \
             CountingTenantPermitResolver fake; an explicit .with_resolver() \
             call has nothing to hand back"
        );
        let counting = CountingTenantPermitResolver::new();
        let params = Self {
            resolver: Some(Arc::clone(&counting) as Arc<dyn AuthZResolverApi>),
            ..self
        };
        let (service, _) = build_service(params, plugin, suffix, Arc::new(NoopMetrics));
        (service, counting)
    }

    /// Build the `Service` against a real metrics adapter bound to a fresh
    /// local `SdkMeterProvider` + `InMemoryMetricExporter` pair, returned
    /// alongside the service.
    #[must_use]
    pub(crate) fn build_with_metrics(
        self,
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> (Arc<Service>, SdkMeterProvider, InMemoryMetricExporter) {
        let (metrics, provider, exporter) = local_metrics();
        let (service, _) = build_service(self, plugin, suffix, metrics);
        (service, provider, exporter)
    }
}

/// The one private full-parameter constructor every [`ServiceFixture`]
/// terminal method funnels through: wires the Type Resolver (inert default
/// or working over `params.source`), the PDP fake (`params.resolver` or a
/// freshly built [`CountingTenantPermitResolver`]), the given `metrics` sink,
/// and `params.cap` (or the default), against `plugin` registered under the
/// `cyberfabric` vendor.
fn build_service(
    params: ServiceFixture,
    plugin: Arc<dyn UsageCollectorPluginV1>,
    suffix: &str,
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> (Arc<Service>, Arc<dyn AuthZResolverApi>) {
    let type_resolver = match params.source {
        Some(source) => type_resolver_over(source, Arc::clone(&metrics)),
        None => inert_type_resolver(Arc::clone(&metrics)),
    };
    let resolver = params.resolver.unwrap_or_else(|| {
        Arc::clone(&CountingTenantPermitResolver::new()) as Arc<dyn AuthZResolverApi>
    });
    let hub = hub_with_plugin(plugin, suffix, "cyberfabric");
    let enforcer = enforcer_for(Arc::clone(&resolver));
    let service = Arc::new(Service::new_with_metrics(
        hub,
        "cyberfabric".to_owned(),
        enforcer,
        metrics,
        type_resolver,
        params
            .cap
            .unwrap_or(crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES),
    ));
    (service, resolver)
}

/// A working Type Resolver over `source`, with a TTL generous enough that a
/// test's several service calls hit the same cached entry rather than
/// re-resolving.
#[must_use]
fn type_resolver_over(
    source: Arc<dyn DeclarationSource>,
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> Arc<TypeResolver> {
    Arc::new(TypeResolver::new(
        source,
        TypeResolverConfig {
            ttl: std::time::Duration::from_mins(1),
            capacity: 16,
        },
        metrics,
    ))
}

// ── Metrics-instrumented Service builders + in-memory readback ──────────────
//
// These wire a `Service` with a real [`UcMetricsMeter`] bound to a local
// `SdkMeterProvider` + `InMemoryMetricExporter`, so emission tests can call a
// service method, `force_flush()` the returned provider, and read back the
// exported instruments. `opentelemetry_sdk` is a dev-dependency; this module
// is test-only.

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use crate::infra::metrics::UcMetricsMeter;

/// Build a fresh local `SdkMeterProvider` + `InMemoryMetricExporter` and a
/// `UcMetricsMeter` (prefix `uc`) bound to it.
#[must_use]
pub fn local_metrics() -> (
    Arc<UcMetricsMeter>,
    SdkMeterProvider,
    InMemoryMetricExporter,
) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = Arc::new(UcMetricsMeter::new(
        &provider.meter("usage-collector"),
        "uc",
    ));
    (metrics, provider, exporter)
}

/// Wire a [`ClientHub`] whose types-registry advertises one usage-collector
/// plugin instance but does **not** register a scoped client under it, so
/// `Service::get_plugin` resolves an instance id yet fails with
/// `PluginUnavailable` — the structural-unready path.
#[must_use]
pub fn hub_registry_only(suffix: &str, vendor: &str) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    let instance_id = usage_collector_instance_id(suffix);
    let instance = make_test_instance(&instance_id, plugin_instance_content(&instance_id, vendor));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);
    hub
}

/// A metrics-instrumented [`Service`] whose plugin binding is structurally
/// unready (registry advertises an instance, no scoped client registered).
#[must_use]
pub fn service_with_metrics_unready_plugin(
    suffix: &str,
    resolver: Arc<dyn AuthZResolverApi>,
) -> (Arc<Service>, SdkMeterProvider, InMemoryMetricExporter) {
    let hub = hub_registry_only(suffix, "cyberfabric");
    let (metrics, provider, exporter) = local_metrics();
    let type_resolver = inert_type_resolver(Arc::clone(&metrics) as Arc<dyn UsageCollectorMetrics>);
    let service = Arc::new(Service::new_with_metrics(
        hub,
        "cyberfabric".to_owned(),
        enforcer_for(resolver),
        metrics,
        type_resolver,
        crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
    ));
    (service, provider, exporter)
}

/// Total summed value of a `u64` counter series, filtered to the data points
/// carrying `label_key == label_value`. Returns `0` when the instrument or
/// label is absent.
#[must_use]
pub fn counter_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

/// Total sample count across all data points of an `f64` histogram. Returns
/// `0` when the instrument is absent.
#[must_use]
pub fn histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

/// Total sample count across the `f64` histogram data points carrying
/// `label_key == label_value`. Returns `0` when the instrument or label is
/// absent. Use this (rather than [`histogram_count`]) when a single call drives
/// several dispatches through the same instrument and the assertion must pin the
/// per-`operation` sample — e.g. proving the emit-path SPI calls contribute a
/// `uc_plugin_call_duration_seconds{operation="create_usage_record"}` sample.
#[must_use]
pub fn histogram_count_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

/// Sum of every value recorded into an `f64` histogram (across all data
/// points). Returns `0.0` when the instrument is absent. Use this — not
/// [`histogram_count`] (sample count) — to pin the observed *magnitude*, e.g.
/// the total bytes recorded into `uc_record_metadata_bytes`.
#[must_use]
pub fn histogram_sum(exporter: &InMemoryMetricExporter, name: &str) -> f64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum)
                        .sum();
                }
            }
        }
    }
    0.0
}

/// Sum of the values recorded into the `f64` histogram data points carrying
/// `label_key == label_value`. Returns `0.0` when the instrument or label is
/// absent. Use this to pin the observed magnitude for a single label series —
/// e.g. the row count recorded into
/// `uc_query_result_rows{query_kind="raw"}` — which
/// [`histogram_count_with_label`] (sample count) cannot.
#[must_use]
pub fn histogram_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> f64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum)
                        .sum();
                }
            }
        }
    }
    0.0
}

/// Last recorded value of an `i64` gauge series. `None` when absent.
#[must_use]
pub fn gauge_last(exporter: &InMemoryMetricExporter, name: &str) -> Option<i64> {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::I64(MetricData::Gauge(g)) = metric.data()
                {
                    return g
                        .data_points()
                        .next()
                        .map(opentelemetry_sdk::metrics::data::GaugeDataPoint::value);
                }
            }
        }
    }
    None
}

/// Build an authenticated [`SecurityContext`] sufficient for PDP requests
/// composed from a [`UsageRecord`]'s attribution tuple.
#[must_use]
pub fn authenticated_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .subject_type("user")
        .build()
        .expect("authenticated context")
}

// ── HappyPathPlugin: programmable Ok-by-default SPI stub ────────────────────

use std::sync::Mutex;

/// The keyset order an SPI dispatch was handed, as `(field, direction)`
/// pairs. Recorded rather than the whole `ODataOrderBy` so an assertion is
/// a plain `assert_eq!` on comparable values — `OrderKey` carries no
/// `PartialEq`.
pub type RecordedOrder = Vec<(String, toolkit_odata::SortDir)>;

/// Per-record outcome shape for a `create_usage_records` SPI batch
/// response — `Ok(persisted_record)` or
/// `Err(UsageCollectorPluginError)`. Factored out so the
/// `HappyPathPlugin` field type and the `set_create_records` parameter
/// type stay readable.
pub type CreateRecordsBatchResult = Vec<Result<UsageRecord, UsageCollectorPluginError>>;

/// Programmable plugin stub that returns the configured response for each
/// SPI method, defaulting to `UsageCollectorPluginError::internal("not
/// programmed")` for methods the test has not explicitly set up. Methods
/// also record their last-seen input so handler-level tests can verify
/// the service forwarded the expected argument.
///
/// The stub is `Arc<Self>` everywhere; interior state lives behind
/// `Mutex` so callers can program responses after construction.
pub struct HappyPathPlugin {
    create_record_response: Mutex<Option<Result<UsageRecord, UsageCollectorPluginError>>>,
    create_records_response: Mutex<Option<CreateRecordsBatchResult>>,
    deactivate_response: Mutex<Option<()>>,
    get_record_response: Mutex<Option<UsageRecord>>,
    list_usage_records_response: Mutex<Option<ODataPage<UsageRecord>>>,
    query_aggregated_usage_records_response: Mutex<Option<AggregationResult>>,
    /// Every [`AggregationFold`] passed to `query_aggregated_usage_records`,
    /// in call order. `len()` is the call count ([`HappyPathPlugin::calls`])
    /// and the last entry is [`HappyPathPlugin::last_fold`] — the
    /// [`RecordingPlugin`] spy shape for the declared-fold tests.
    query_aggregated_usage_records_folds: Mutex<Vec<AggregationFold>>,

    create_record_input: Mutex<Option<UsageRecord>>,
    create_records_input: Mutex<Option<Vec<UsageRecord>>>,
    deactivate_input: Mutex<Option<Uuid>>,
    /// Every record `id` ever passed to `get_usage_record`, in call
    /// order. Drives the L1-corrects-id dedup tests.
    get_usage_record_inputs: Mutex<Vec<Uuid>>,
    /// Record `id`s that should surface as
    /// `UsageCollectorPluginError::UsageRecordNotFound` instead of
    /// returning the default `get_record_response`.
    get_usage_record_not_found: Mutex<std::collections::BTreeSet<Uuid>>,
    /// `Debug` rendering of the most-recent `scope` passed to
    /// `get_usage_record`, so a test can assert the plugin actually
    /// received a compiled PDP scope (Task 13 / DESIGN §3.3) rather than
    /// merely that the call succeeded.
    last_get_scope: Mutex<Option<String>>,

    /// The `time_range` passed to the most-recent `list_usage_records`
    /// dispatch. The range is a typed parameter rather than a `$filter`
    /// conjunct, so nothing in the `ODataQuery` a test inspects would
    /// reveal a range the gateway dropped on the way to the SPI — and a
    /// dropped range is an unbounded scan that still answers `200`.
    list_time_range: Mutex<Option<TimeRange>>,
    /// The same recorder for `query_aggregated_usage_records`.
    aggregate_time_range: Mutex<Option<TimeRange>>,
    /// The `query.order` the most-recent `list_usage_records` dispatch was
    /// handed, as `(field, direction)` pairs. The SPI documents this slot
    /// as a populated, uniform-direction, never-null keyset on every
    /// surface, and nothing in a returned page would show an order the
    /// gateway failed to floor — an unfloored order is a keyset the plugin
    /// cannot continue, or silently drops rows from.
    list_order: Mutex<Option<RecordedOrder>>,
}

impl HappyPathPlugin {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            create_record_response: Mutex::new(None),
            create_records_response: Mutex::new(None),
            deactivate_response: Mutex::new(None),
            get_record_response: Mutex::new(None),
            list_usage_records_response: Mutex::new(None),
            query_aggregated_usage_records_response: Mutex::new(None),
            query_aggregated_usage_records_folds: Mutex::new(Vec::new()),
            create_record_input: Mutex::new(None),
            create_records_input: Mutex::new(None),
            deactivate_input: Mutex::new(None),
            get_usage_record_inputs: Mutex::new(Vec::new()),
            get_usage_record_not_found: Mutex::new(std::collections::BTreeSet::new()),
            last_get_scope: Mutex::new(None),
            list_time_range: Mutex::new(None),
            aggregate_time_range: Mutex::new(None),
            list_order: Mutex::new(None),
        })
    }

    pub fn set_create_record(&self, record: UsageRecord) {
        *self.create_record_response.lock().expect("mutex") = Some(Ok(record));
    }
    /// Program the singular `create_usage_record` SPI to return `err` on
    /// the next call. Used by tests that need to drive plugin-side
    /// failure modes (`Transient`, `IdempotencyConflict`, …) into the
    /// service's singular path.
    pub fn set_create_record_err(&self, err: UsageCollectorPluginError) {
        *self.create_record_response.lock().expect("mutex") = Some(Err(err));
    }
    pub fn set_create_records(&self, results: CreateRecordsBatchResult) {
        *self.create_records_response.lock().expect("mutex") = Some(results);
    }
    pub fn set_deactivate_ok(&self) {
        *self.deactivate_response.lock().expect("mutex") = Some(());
    }
    pub fn set_get_record(&self, record: UsageRecord) {
        *self.get_record_response.lock().expect("mutex") = Some(record);
    }
    /// Mark `id` so the next (and every subsequent) `get_usage_record`
    /// call carrying it returns `UsageRecordNotFound` regardless of the
    /// default `get_record_response`.
    pub fn set_get_usage_record_not_found(&self, id: Uuid) {
        self.get_usage_record_not_found
            .lock()
            .expect("mutex")
            .insert(id);
    }
    /// Every record `id` passed to `get_usage_record`, in call order.
    #[must_use]
    pub fn get_usage_record_inputs(&self) -> Vec<Uuid> {
        self.get_usage_record_inputs.lock().expect("mutex").clone()
    }
    /// Total number of `get_usage_record` SPI dispatches so far.
    #[must_use]
    pub fn get_usage_record_calls(&self) -> usize {
        self.get_usage_record_inputs.lock().expect("mutex").len()
    }
    /// `Debug` rendering of the `scope` filter passed to the most-recent
    /// `get_usage_record` call, or `None` if it was never invoked. Proves
    /// the caller-facing point lookup actually handed the plugin a
    /// compiled PDP scope (Task 13 / DESIGN §3.3), not merely that the
    /// call returned `Ok`.
    #[must_use]
    pub fn last_get_scope(&self) -> Option<String> {
        self.last_get_scope.lock().expect("mutex").clone()
    }
    pub fn set_list_usage_records_response(&self, page: ODataPage<UsageRecord>) {
        *self.list_usage_records_response.lock().expect("mutex") = Some(page);
    }
    pub fn set_query_aggregated_usage_records_response(&self, result: AggregationResult) {
        *self
            .query_aggregated_usage_records_response
            .lock()
            .expect("mutex") = Some(result);
    }
    /// Total number of `query_aggregated_usage_records` SPI dispatches so
    /// far — the [`RecordingPlugin`] spy's call counter.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.query_aggregated_usage_records_folds
            .lock()
            .expect("mutex")
            .len()
    }
    /// The most-recent [`AggregationFold`] passed to
    /// `query_aggregated_usage_records`, or `None` if it was never invoked.
    #[must_use]
    pub fn last_fold(&self) -> Option<AggregationFold> {
        self.query_aggregated_usage_records_folds
            .lock()
            .expect("mutex")
            .last()
            .copied()
    }
    /// The [`TimeRange`] handed to the most-recent `list_usage_records`
    /// dispatch, or `None` if it was never invoked. Proves the range
    /// survived the gateway as a typed parameter, not merely that the read
    /// returned `Ok`.
    #[must_use]
    pub fn last_list_time_range(&self) -> Option<TimeRange> {
        *self.list_time_range.lock().expect("mutex")
    }
    /// The [`TimeRange`] handed to the most-recent
    /// `query_aggregated_usage_records` dispatch, or `None` if it was never
    /// invoked.
    #[must_use]
    pub fn last_aggregate_time_range(&self) -> Option<TimeRange> {
        *self.aggregate_time_range.lock().expect("mutex")
    }
    /// The keyset order handed to the most-recent `list_usage_records`
    /// dispatch as `(field, direction)` pairs, or `None` if it was never
    /// invoked. Proves the gateway floored the order the SPI requires,
    /// rather than merely that the read returned `Ok`.
    #[must_use]
    pub fn last_list_order(&self) -> Option<RecordedOrder> {
        self.list_order.lock().expect("mutex").clone()
    }
    pub fn last_create_record_input(&self) -> Option<UsageRecord> {
        self.create_record_input.lock().expect("mutex").clone()
    }
    pub fn last_create_records_input(&self) -> Option<Vec<UsageRecord>> {
        self.create_records_input.lock().expect("mutex").clone()
    }
    pub fn last_deactivate_input(&self) -> Option<Uuid> {
        *self.deactivate_input.lock().expect("mutex")
    }
}

fn not_programmed(method: &'static str) -> UsageCollectorPluginError {
    UsageCollectorPluginError::internal(format!("HappyPathPlugin::{method} not programmed"))
}

#[async_trait]
impl UsageCollectorPluginV1 for HappyPathPlugin {
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        *self.create_record_input.lock().expect("mutex") = Some(record);
        // `take()` (not `clone()`) — `UsageCollectorPluginError` is
        // intentionally `!Clone` so tests program one outcome per call.
        match self.create_record_response.lock().expect("mutex").take() {
            Some(outcome) => outcome,
            None => Err(not_programmed("create_usage_record")),
        }
    }

    async fn create_usage_records(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        *self.create_records_input.lock().expect("mutex") = Some(records);
        self.create_records_response
            .lock()
            .expect("mutex")
            .take()
            .ok_or_else(|| not_programmed("create_usage_records"))
    }

    async fn query_aggregated_usage_records(
        &self,
        _gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        *self.aggregate_time_range.lock().expect("mutex") = Some(time_range);
        self.query_aggregated_usage_records_folds
            .lock()
            .expect("mutex")
            .push(fold);
        self.query_aggregated_usage_records_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("query_aggregated_usage_records"))
    }

    async fn list_usage_records(
        &self,
        _gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        *self.list_time_range.lock().expect("mutex") = Some(time_range);
        *self.list_order.lock().expect("mutex") = Some(
            query
                .order
                .0
                .iter()
                .map(|key| (key.field.clone(), key.dir))
                .collect(),
        );
        self.list_usage_records_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("list_usage_records"))
    }

    async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
        *self.deactivate_input.lock().expect("mutex") = Some(id);
        self.deactivate_response
            .lock()
            .expect("mutex")
            .ok_or_else(|| not_programmed("deactivate_usage_record"))
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.get_usage_record_inputs.lock().expect("mutex").push(id);
        *self.last_get_scope.lock().expect("mutex") = Some(format!("{scope:?}"));
        if self
            .get_usage_record_not_found
            .lock()
            .expect("mutex")
            .contains(&id)
        {
            return Err(UsageCollectorPluginError::UsageRecordNotFound { id });
        }
        self.get_record_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("get_usage_record"))
    }
}

/// Spy alias over [`HappyPathPlugin`] for the declared-fold aggregate tests
/// (Tasks 8, 9, 12, 13): [`HappyPathPlugin::calls`] and
/// [`HappyPathPlugin::last_fold`] already record every
/// `query_aggregated_usage_records` dispatch, so this is a naming alias
/// rather than a second spy type.
pub(crate) type RecordingPlugin = HappyPathPlugin;

// ── DeclarationSource fakes: fold / metadata / not-found / counting ────────

use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::USAGE_RECORD_BASE_TYPE;

use crate::domain::error::DomainError;
use crate::domain::ports::declarations::DeclarationSource;
use crate::domain::ports::metrics::{NoopMetrics, UsageCollectorMetrics};

/// Builds a base + single-derived-meter `GtsTypeSchema` pair for `id`,
/// declaring `x-gts-traits` at the schema's **top level** (never nested in
/// `allOf` — that is the placement `GtsTypeSchema::effective_traits` reads;
/// see `type_resolver::declaration_tests` for the history) with
/// `aggregation_fold: fold`, `canonical_unit: "bytes"`, and a closed
/// `metadata` surface admitting exactly `metadata_keys`.
fn fake_meter_schema(id: &MeterTypeId, fold: &str, metadata_keys: &[String]) -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(USAGE_RECORD_BASE_TYPE).expect("base type id"),
        serde_json::json!({
            "type": "object",
            "x-gts-abstract": true,
            "properties": {
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } }
            }
        }),
        None,
        None,
    )
    .expect("base schema");

    let properties: serde_json::Map<String, serde_json::Value> = metadata_keys
        .iter()
        .map(|key| (key.clone(), serde_json::json!({ "type": "string" })))
        .collect();

    GtsTypeSchema::try_new(
        id.as_gts().clone(),
        serde_json::json!({
            "allOf": [{ "$ref": format!("gts://{USAGE_RECORD_BASE_TYPE}") }],
            "x-gts-traits": {
                "aggregation_fold": fold,
                "canonical_unit": "bytes"
            },
            "properties": {
                "metadata": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": properties
                }
            }
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("derived schema")
}

/// A [`DeclarationSource`] that always resolves — regardless of the `id` it
/// is asked to `fetch` — to a meter declaring `fold`, unit `bytes`, and the
/// given closed `metadata_keys` surface.
struct FakeDeclarationSource {
    fold: String,
    metadata_keys: Vec<String>,
}

#[async_trait]
impl DeclarationSource for FakeDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        Ok(fake_meter_schema(id, &self.fold, &self.metadata_keys))
    }
}

/// A `DeclarationSource` that always resolves to a meter declaring `fold`,
/// unit `bytes` and no metadata properties.
#[must_use]
pub(crate) fn fake_declaration_source_with_fold(fold: &str) -> Arc<dyn DeclarationSource> {
    Arc::new(FakeDeclarationSource {
        fold: fold.to_owned(),
        metadata_keys: Vec::new(),
    })
}

/// A `DeclarationSource` resolving to a meter whose metadata surface declares
/// exactly `keys`.
#[must_use]
pub(crate) fn fake_declaration_source_with_metadata(keys: &[&str]) -> Arc<dyn DeclarationSource> {
    Arc::new(FakeDeclarationSource {
        fold: "SUM".to_owned(),
        metadata_keys: keys.iter().map(|k| (*k).to_owned()).collect(),
    })
}

/// A [`DeclarationSource`] that always answers a definite not-found —
/// [`DomainError::is_declaration_not_found`] reports `true` for it, so the
/// Type Resolver fails closed immediately rather than treating it as a
/// possibly-transient miss.
struct NotFoundDeclarationSource;

#[async_trait]
impl DeclarationSource for NotFoundDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        Err(DomainError::declaration_not_found(id))
    }
}

/// A `DeclarationSource` that always answers a definite not-found.
#[must_use]
pub(crate) fn fake_declaration_source_not_found() -> Arc<dyn DeclarationSource> {
    Arc::new(NotFoundDeclarationSource)
}

/// A [`DeclarationSource`] counting its `fetch` calls (and recording the id
/// each one resolved), so a batch test can assert one resolution per
/// distinct meter rather than one per record. Resolves every `id` the same
/// way [`FakeDeclarationSource`] does (fold `SUM`, unit `bytes`, no metadata
/// properties) — except `unresolvable`, when set, which always answers a
/// definite not-found regardless of `id`: the mixed-batch analogue of
/// [`fake_declaration_source_not_found`], for a test driving "one distinct
/// type resolves, another does not" in the same batch. Merges what used to
/// be two ~90%-identical types (`CountingDeclarationSource` and
/// `PartiallyUnresolvableDeclarationSource`) into this one optional field.
pub(crate) struct CountingDeclarationSource {
    inner: FakeDeclarationSource,
    calls: AtomicUsize,
    inputs: Mutex<Vec<MeterTypeId>>,
    unresolvable: Option<MeterTypeId>,
}

impl CountingDeclarationSource {
    /// Total number of `fetch` calls observed so far.
    #[must_use]
    pub(crate) fn fetch_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every id passed to `fetch`, in call order.
    #[must_use]
    pub(crate) fn fetch_inputs(&self) -> Vec<MeterTypeId> {
        self.inputs.lock().expect("mutex").clone()
    }
}

#[async_trait]
impl DeclarationSource for CountingDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().expect("mutex").push(id.clone());
        if self.unresolvable.as_ref() == Some(id) {
            return Err(DomainError::declaration_not_found(id));
        }
        self.inner.fetch(id).await
    }
}

/// A `DeclarationSource` resolving every id to a meter declaring `SUM` /
/// `bytes` / no metadata properties, except `unresolvable`, which always
/// answers a definite not-found.
#[must_use]
pub(crate) fn fake_declaration_source_with_one_unresolvable(
    unresolvable: MeterTypeId,
) -> Arc<CountingDeclarationSource> {
    Arc::new(CountingDeclarationSource {
        inner: FakeDeclarationSource {
            fold: "SUM".to_owned(),
            metadata_keys: Vec::new(),
        },
        calls: AtomicUsize::new(0),
        inputs: Mutex::new(Vec::new()),
        unresolvable: Some(unresolvable),
    })
}

/// A `DeclarationSource` counting its calls, so a batch test can assert one
/// resolution per distinct meter rather than one per record.
#[must_use]
pub(crate) fn fake_declaration_source_counting() -> Arc<CountingDeclarationSource> {
    Arc::new(CountingDeclarationSource {
        inner: FakeDeclarationSource {
            fold: "SUM".to_owned(),
            metadata_keys: Vec::new(),
        },
        calls: AtomicUsize::new(0),
        inputs: Mutex::new(Vec::new()),
        unresolvable: None,
    })
}

/// Registration suffix every `RecordingPlugin`-backed test builds its
/// [`Service`] under. Fixed rather than caller-supplied — each test wires
/// its own fresh [`ClientHub`] (see [`hub_with_plugin`]), so a shared
/// instance id across tests never collides.
pub(crate) const RECORDING_PLUGIN_SUFFIX: &str = "test.usage_collector.recording.plugin.v1";

/// The PDP fake a `RecordingPlugin`-backed [`ServiceFixture`] must use
/// (`.with_resolver(recording_plugin_resolver())`) instead of the default
/// [`CountingTenantPermitResolver`].
///
/// The aggregate path's PDP request carries no per-instance resource
/// properties (it authorizes pre-row, under `require_constraints(true)`),
/// so the enforcer needs [`CountingPermitResolver`] — a fixed
/// `OWNER_TENANT_ID` constraint returned regardless of the request shape —
/// rather than [`CountingTenantPermitResolver`] (which reads the constraint
/// back out of the request and would see no tenant key here, falling back to
/// an allow-all permit the aggregate gate then denies).
#[must_use]
pub(crate) fn recording_plugin_resolver() -> Arc<dyn AuthZResolverApi> {
    CountingPermitResolver::new(
        pep_properties::OWNER_TENANT_ID,
        Uuid::from_u128(2).to_string(),
    )
}
