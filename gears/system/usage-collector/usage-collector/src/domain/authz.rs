//! PEP gate and per-resource vocabulary for the usage-collector domain.
//!
//! Per `cpt-cf-usage-collector-adr-pdp-centric-authorization` the collector
//! keeps NO local policy table and NO PDP-decision cache; every decision is
//! delegated to the bound `authz-resolver` client. The ingestion surface
//! declares per-record attribution attributes so policy can reason over them,
//! and runs under `require_constraints(true)`, gating each record's owning
//! tenant against the PDP-returned row scope.
//!
//! Fail-closed wiring (transport → `AuthorizationUnavailable`, deny /
//! compile-failed → `AuthorizationDenied`) lives here so it cannot drift
//! between call sites.

use authz_resolver_sdk::pep::{AccessRequest, ResourceType};
use authz_resolver_sdk::{EnforcerError, PolicyEnforcer};
use toolkit_macros::domain_model;
use toolkit_odata::ast;
use toolkit_security::{
    AccessScope, ScopeConstraint, ScopeFilter, ScopeValue, SecurityContext, pep_properties,
};
use usage_collector_sdk::UsageRecord;
use uuid::Uuid;

use crate::domain::ports::metrics::{AuthzDecision, PdpFailureCause, PdpOp, UsageCollectorMetrics};

use super::error::DomainError;

/// Single shared instrumentation point around
/// [`PolicyEnforcer::access_scope_with`] — the sole realization site for
/// DESIGN §3.11.5's PDP-helper instruments (`uc_pdp_ready`,
/// `uc_pdp_duration_seconds`, `uc_authz_decisions_total`,
/// `uc_pdp_failures_total`). Every helper in this module routes through it so
/// instrumentation cannot drift between the per-record and query call sites.
///
/// **`uc_authz_decisions_total` records the EFFECTIVE gear decision, not the
/// raw `access_scope_with` return.** Under `require_constraints(true)` a permit
/// comes back as a permit-with-constraints (`Ok(scope)`) that the SDK does NOT
/// auto-match against the request; the gear then applies a per-call `gate`
/// (the per-record attribution gate, or the scope→`OData` projection) that can
/// turn that constrained permit into a fail-closed deny. Recording off the raw
/// `Ok` would count a cross-tenant attribution attempt — the reconnaissance
/// signal the deny-anomaly alert (DESIGN §3.11.6) keys off — as a `permit`. So
/// a `permit` sample is emitted only after `gate` admits; a gate rejection
/// records `deny`.
///
/// Classification (matching `cpt-cf-usage-collector-algo-pdp-scope-evaluation`),
/// each case also observing duration: `Ok(scope)` with `gate` admitting →
/// permit; `Ok(scope)` with `gate` rejecting → deny; `Denied` / `CompileFailed`
/// → deny (a fail-closed compile failure is a deny, not a failure);
/// `EvaluationFailed` → `uc_pdp_failures_total{cause="unreachable"}`. Exactly
/// one of the decision/failure counters fires per call, so the deny-anomaly
/// ratio can never double-count a single authorization.
///
/// `gate` is a synchronous post-permit predicate mapping the granted
/// [`AccessScope`] to the caller's success value, or to a
/// [`DomainError::AuthorizationDenied`] when the record / query falls outside
/// the grant. The wrapper owns the `EnforcerError` → [`DomainError`] mapping.
// The wrapper mirrors `PolicyEnforcer::access_scope_with`'s parameter list plus
// the metrics sink, operation label, and post-permit gate — bundling them into
// a struct would obscure the 1:1 mapping to the wrapped call.
// @cpt-algo:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-shared-scope-helper:p1
#[allow(clippy::too_many_arguments)]
async fn pdp_scope_with<T>(
    enforcer: &PolicyEnforcer,
    metrics: &dyn UsageCollectorMetrics,
    op: PdpOp,
    ctx: &SecurityContext,
    resource: &ResourceType,
    action: &str,
    resource_id: Option<Uuid>,
    request: &AccessRequest,
    gate: impl FnOnce(AccessScope) -> Result<T, DomainError>,
) -> Result<T, DomainError> {
    // @cpt-algo:cpt-cf-usage-collector-algo-readiness-signal-derivation:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-readiness-signals:p2
    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-ready-gauge
    // The authz-resolver client is bound once at bootstrap inside the
    // `PolicyEnforcer`, so this is a constant post-bootstrap readiness fact;
    // reflecting it here keeps the gauge live even if bootstrap seeding is
    // ever removed. The monotonic start instant scopes `uc_pdp_duration_seconds`.
    metrics.set_pdp_ready(true);
    let start = std::time::Instant::now();
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-ready-gauge
    let result = enforcer
        .access_scope_with(ctx, resource, action, resource_id, request)
        .await;
    // `seconds` is the PDP round-trip only — captured before `gate` runs, so the
    // cheap CPU-side gate never inflates `uc_pdp_duration_seconds`.
    let seconds = start.elapsed().as_secs_f64();
    match result {
        // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-decision-metrics
        Ok(scope) => match gate(scope) {
            // The effective decision: a permit is recorded only once the gear's
            // post-permit gate has admitted the record / query.
            Ok(value) => {
                metrics.record_pdp_decision(op, AuthzDecision::Permit, seconds);
                Ok(value)
            }
            // A constrained permit the gate rejects (cross-tenant attribution,
            // an un-projectable row scope) is a fail-closed deny.
            Err(denied) => {
                metrics.record_pdp_decision(op, AuthzDecision::Deny, seconds);
                Err(denied)
            }
        },
        Err(err @ (EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_))) => {
            metrics.record_pdp_decision(op, AuthzDecision::Deny, seconds);
            Err(DomainError::from(err))
        }
        // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-decision-metrics
        // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-pdp-catch-reject
        // v1: `AuthZResolverError` carries no timeout discriminator and no
        // host-side PDP deadline exists, so every failure is `unreachable`.
        Err(err @ EnforcerError::EvaluationFailed(_)) => {
            metrics.record_pdp_failure(op, PdpFailureCause::Unreachable, seconds);
            Err(DomainError::from(err))
        } // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-pdp-catch-reject
    }
}

/// The full attribution tuple that determines a `UsageRecord` PDP request
/// under a fixed `(SecurityContext, action)` pair.
///
/// **Why this type exists.** The batch ingestion path
/// (`Service::create_usage_records`) deduplicates PDP round-trips by grouping
/// records with byte-identical PDP payloads. Correctness of that dedup hinges
/// on one invariant: **every field the PDP payload reads MUST be carried by
/// this type, and every field this type carries MUST be read by the payload
/// composer.** If those sets diverge, records the PDP would have judged
/// differently could silently share a decision — a bypass.
///
/// The invariant is enforced **structurally**: the only PDP-composer entry
/// point ([`authorize_attribution_tuple`]) takes `&AttributionTupleKey` and
/// nothing else, so it physically cannot reference any record field outside
/// this struct. A new PEP attribute therefore needs a field here, an update to
/// [`AttributionTupleKey::from_record`], and a `.resource_property(...)` line
/// in the composer.
///
/// `action` participates in the hash/eq contract so a batch carrying records
/// bound to different actions cannot collapse onto a single PDP decision. The
/// backfill route is the caller that mixes them: it picks each entry's verb
/// from that entry's own covered period
/// ([`crate::domain::covered_period::ingestion_action`]), so one batch spanning
/// the configured backfill window carries `create` and `backfill` together.
/// Drop `action` from the key and the entry reaching past the window rides in
/// on the other's `create` permit.
///
/// **Realizes one instance of
/// `cpt-cf-usage-collector-algo-opaque-identifier-handling`'s per-value
/// handling rule** on the identifier fields this struct carries. The opacity
/// claim is about this struct's own code, not about the field types: what holds
/// is that the derive list adds nothing beyond
/// `Eq`/`Hash`/`PartialEq`/`Clone`/`Debug`, and no line of this crate's
/// production source calls a decomposing method on a value bound to one of
/// these field names — see
/// `data_classification_tests::the_gear_never_decomposes_an_identifier_it_carries`
/// (named rather than linked, since linking a private `#[cfg(test)]` item warns
/// under `rustdoc::private_intra_doc_links`).
///
/// **This struct backs the algo only, not
/// `cpt-cf-usage-collector-dod-opaque-identifier-treatment`**, whose first
/// sentence binds the ingestion, query, feed and backfill surfaces alike:
/// `AttributionTupleKey` is an authz-internal PDP-batching key on the ingestion
/// path, so it cannot establish that all-surface claim. The algo's "no reverse
/// lookup" and "no enrich" steps are realized at `UsageCollectorModule`'s
/// dependency declarations (`module.rs`).
// @cpt-algo:cpt-cf-usage-collector-algo-opaque-identifier-handling:p3
#[domain_model]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AttributionTupleKey {
    tenant_id: Uuid,
    gts_type_id: String,
    resource_type: String,
    resource_id: String,
    subject_id: Option<String>,
    subject_type: Option<String>,
    action: &'static str,
}

impl AttributionTupleKey {
    /// Extract the tuple key from a record's caller-supplied attribution
    /// fields together with the PEP `action` the batch is authorising. The
    /// fields read here MUST match the `resource_property` / action writes in
    /// [`authorize_attribution_tuple`] — that mirror is load-bearing.
    pub(crate) fn from_record(record: &UsageRecord, action: &'static str) -> Self {
        let (subject_id, subject_type) = match record.subject_ref.as_ref() {
            Some(s) => (
                Some(s.subject_id().to_owned()),
                s.subject_type().map(str::to_owned),
            ),
            None => (None, None),
        };
        Self {
            tenant_id: record.tenant_id,
            // Part of the key, so a batch mixing two meters under one tenant
            // and resource cannot collapse onto a single PDP decision — a scope
            // narrowed to one would otherwise admit the other on its permit.
            gts_type_id: record.gts_type_id.as_ref().to_owned(),
            resource_type: record.resource_ref.resource_type().to_owned(),
            resource_id: record.resource_ref.resource_id().to_owned(),
            subject_id,
            subject_type,
            action,
        }
    }
}

/// PEP vocabulary for the `UsageRecord` ingestion surface.
///
/// The PDP authorizes the subject together with the caller-supplied
/// attribution composites carried on each record: the owning tenant
/// (`UsageRecord::tenant_id` — caller-supplied, never derived from the
/// [`SecurityContext`]), the referenced GTS type (`gts_type_id`, the meter),
/// the optional subject reference, and the mandatory resource reference.
/// Property keys are exported as `PROP_*` constants so call sites and policy
/// authors share one vocabulary.
///
/// Every one of them is advertised. All but the meter also narrow a read; the
/// meter is the exception, for the reason on
/// [`usage_record::PROP_GTS_TYPE_ID`] and [`pep_property_to_field`]. Both links
/// are written from this doc comment's own scope (`domain::authz`, not the
/// module below), because an outer `///` on a `mod` resolves in the parent.
pub(crate) mod usage_record {
    use authz_resolver_sdk::pep::ResourceType;
    use toolkit_security::pep_properties;
    use usage_collector_sdk::USAGE_RECORD_RESOURCE;

    /// PEP attribute key carrying the caller-supplied `resource_type`.
    pub const PROP_RESOURCE_TYPE: &str = "resource_type";

    /// PEP attribute key carrying the caller-supplied `resource_id`.
    pub const PROP_RESOURCE_ID: &str = "resource_id";

    /// PEP attribute key carrying the optional caller-supplied `subject_type`
    /// qualifier (present only when [`usage_collector_sdk::SubjectRef`] is
    /// supplied AND its `subject_type` field is populated).
    pub const PROP_SUBJECT_TYPE: &str = "subject_type";

    /// PEP attribute key carrying the caller-supplied `gts_type_id` — the
    /// meter an entry is measured against.
    ///
    /// **Advertised, but honoured on the attribution tuple only.** The
    /// per-record gate compares it
    /// ([`super::scope_admits_attribution_tuple`]); the `OData` projection
    /// ([`super::pep_property_to_field`]) refuses it by name on every path that
    /// projects a scope — the read paths **and** the invalidation-target lookup
    /// on ingestion, so a grant narrowed by meter can measure but cannot
    /// withdraw. Read those sites before making the two agree.
    pub const PROP_GTS_TYPE_ID: &str = "gts_type_id";

    /// PEP resource type for the `UsageRecord` ingestion surface. Declares the
    /// attribution-tuple attributes the PDP may key its policy on.
    // @cpt-dod:cpt-cf-usage-collector-dod-advertised-scope-properties:p1
    pub const RESOURCE: ResourceType = ResourceType::from_static(
        USAGE_RECORD_RESOURCE,
        &[
            pep_properties::OWNER_TENANT_ID,
            pep_properties::OWNER_ID,
            PROP_RESOURCE_TYPE,
            PROP_RESOURCE_ID,
            PROP_SUBJECT_TYPE,
            PROP_GTS_TYPE_ID,
        ],
    );

    /// `UsageRecord` action vocabulary. Renaming any of these is a contract
    /// change against the PDP policy bundle.
    pub mod actions {
        pub const CREATE: &str = "create";
        pub const GET: &str = "get";
        pub const LIST: &str = "list";
        /// Import or withdraw a covered period ending further back than the
        /// configured backfill window.
        ///
        /// The elevated grant of
        /// `cpt-cf-usage-collector-adr-backfill-isolation`, and **not** the
        /// backfill route's action — an entry on that route whose period ends
        /// inside the window authorizes [`CREATE`], needing no privilege a live
        /// emission does not. This one is granted to an import job, so an
        /// emitter that finds a gap older than the window cannot close it.
        pub const BACKFILL: &str = "backfill";

        /// Read a page of the usage feed.
        ///
        /// Distinct from [`LIST`] deliberately:
        /// `cpt-cf-usage-collector-adr-feed-aggregate-split` separates a
        /// charging consumer from an audit reader, and two PEP actions are what
        /// let a deployment grant one without the other. Reusing `LIST` here
        /// would make any grant on the audit path confer the charging feed.
        ///
        /// **Deployment note:** a deployment upgrading past the Feed Gateway
        /// must add this grant to its PDP policy bundle, or every feed
        /// request is denied.
        pub const READ_FEED: &str = "read_feed";

        /// Read per-scope reconciliation metadata.
        ///
        /// A PEP verb distinct from [`LIST`] (DESIGN §3.9.6). The
        /// reconciliation surface is operator-only and reports counters and
        /// watermarks rather than entries, so an operator clears this gate
        /// without being granted the entry-level audit path — and a tenant
        /// administrator holding `list` does not reach the operator surface.
        pub const RECONCILE: &str = "reconcile";
    }
}

/// Run the PDP check for an [`AttributionTupleKey`], the complete description
/// of a `UsageRecord`'s ingestion PDP payload.
///
/// This is the sole composer of the `UsageRecord` PDP request. By taking
/// `&AttributionTupleKey` and no `&UsageRecord`, it makes "the dedup grouping
/// key is a complete description of the PDP payload" a **structural**
/// invariant — see [`AttributionTupleKey`]. The only projection from a record
/// is [`AttributionTupleKey::from_record`], which the batch ingestion path runs
/// once per group.
///
/// The key's `action` selects the verb, whatever
/// [`crate::domain::covered_period::ingestion_action`] picked per entry.
///
/// Unlike the query-path helpers, which project their constraints into an
/// `OData` filter, this path applies the per-record attribution gate in
/// [`scope_admits_attribution_tuple`], which is where the fail-closed posture
/// for a constrained permit is stated.
///
/// On a permit, returns the granted [`AccessScope`]: an invalidation's target
/// lookup reads under it, compiled by [`scope_to_odata_filter`].
///
/// # Errors
///
/// * [`DomainError::AuthorizationDenied`] when the PDP denies, returns an
///   uncompilable constraint shape, or grants a scope that does not cover
///   the record's owning tenant.
/// * [`DomainError::AuthorizationUnavailable`] when the PDP transport fails.
// @cpt-algo:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1
// @cpt-dod:cpt-cf-usage-collector-fr-tenant-attribution:p1
// @cpt-dod:cpt-cf-usage-collector-fr-resource-attribution:p1
// @cpt-dod:cpt-cf-usage-collector-fr-subject-attribution:p1
// @cpt-dod:cpt-cf-usage-collector-fr-ingestion-authorization:p1
// @cpt-dod:cpt-cf-usage-collector-entity-model:p1
// @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p1
// @cpt-dod:cpt-cf-usage-collector-adr-caller-supplied-attribution:p1
pub(crate) async fn authorize_attribution_tuple(
    enforcer: &PolicyEnforcer,
    metrics: &dyn UsageCollectorMetrics,
    op: PdpOp,
    ctx: &SecurityContext,
    key: &AttributionTupleKey,
) -> Result<AccessScope, DomainError> {
    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-pdp-inputs
    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-compose
    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-attrib-compose-tuple
    let mut request = AccessRequest::new()
        .require_constraints(true)
        .resource_property(pep_properties::OWNER_TENANT_ID, key.tenant_id.to_string())
        // The referenced GTS type travels on the request, not merely in the
        // gate: DESIGN §3.5's Platform PDP row names it in the payload, and
        // [`AttributionTupleKey`]'s invariant requires every field the key
        // carries to be read here.
        .resource_property(usage_record::PROP_GTS_TYPE_ID, key.gts_type_id.clone())
        .resource_property(usage_record::PROP_RESOURCE_TYPE, key.resource_type.clone())
        .resource_property(usage_record::PROP_RESOURCE_ID, key.resource_id.clone());

    if let Some(subject_id) = key.subject_id.as_ref() {
        request = request.resource_property(pep_properties::OWNER_ID, subject_id.clone());
        if let Some(subject_type) = key.subject_type.as_ref() {
            request =
                request.resource_property(usage_record::PROP_SUBJECT_TYPE, subject_type.clone());
        }
    }
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-attrib-compose-tuple
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-compose
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-pdp-inputs

    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-pdp-helper
    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-call
    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-attrib-pdp-deny
    // @cpt-begin:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-attrib-pdp-allow
    // Per-record attribution gate (see [`scope_admits_attribution_tuple`]),
    // applied as the wrapper's post-permit gate. Running inside
    // `pdp_scope_with` is what makes a rejection here record
    // `uc_authz_decisions_total{decision="deny"}` rather than the raw permit.
    pdp_scope_with(
        enforcer,
        metrics,
        op,
        ctx,
        &usage_record::RESOURCE,
        key.action,
        None,
        &request,
        |scope| match scope_admits_attribution_tuple(&scope, key) {
            Ok(()) => Ok(scope),
            // Name the attribute that actually rejected. Any narrowing
            // predicate the scope carries can reject here — the meter,
            // `resource_id`, `resource_type`, the subject — and reporting all
            // of them as a tenant failure sends an operator to the wrong
            // policy.
            Err(rejection) => {
                let (rejected_on, message, reason) = match &rejection {
                    TupleRejection::Tenant => (
                        pep_properties::OWNER_TENANT_ID.to_owned(),
                        "PDP permit did not authorize the record's owning tenant; \
                         denying cross-tenant usage_record attribution",
                        format!(
                            "caller is not authorized for usage_record owning tenant {}",
                            key.tenant_id
                        ),
                    ),
                    TupleRejection::Property(property) => (
                        property.clone(),
                        "PDP permit narrowed the grant by an attribution property the \
                         record does not satisfy; denying out-of-grant usage_record \
                         attribution",
                        format!(
                            "caller's usage_record grant for tenant {} does not admit \
                             this entry's `{property}`",
                            key.tenant_id
                        ),
                    ),
                };
                tracing::warn!(
                    target: "authz",
                    tenant_id = %key.tenant_id,
                    action = key.action,
                    rejected_on = %rejected_on,
                    "{message}"
                );
                Err(DomainError::AuthorizationDenied {
                    reason: Some(reason),
                })
            }
        },
    )
    .await
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-attrib-pdp-allow
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-attrib-pdp-deny
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-algo-pdp-call
    // @cpt-end:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1:inst-pdp-helper
}

/// Decide whether a PDP-returned [`AccessScope`] authorizes a per-record
/// operation on the record described by `key`.
///
/// [`authorize_attribution_tuple`] runs under `require_constraints(true)`, so
/// a permit comes back as a compiled scope rather than a bare yes/no, and the
/// SDK does NOT auto-match that scope against the request's resource
/// properties — confirming the record falls inside the granted scope is the
/// gear's responsibility. Without this check a `/tenants/{A}`-scoped caller
/// could create or read records attributed to any other tenant: the resolver
/// returns a permit plus an `OWNER_TENANT_ID In [A's closure]` narrowing, but
/// nothing otherwise rejects an out-of-closure record.
///
/// Constraints are OR-ed (one per independent access path) and the filters
/// within a constraint are AND-ed — the same shape [`scope_to_odata_filter`]
/// projects. This gate is the point-operation analogue of that projection:
/// rather than pushing the scope into a query, it evaluates the single record's
/// attribution tuple against it directly. **A constraint admits the record iff**
/// every one of its filters is satisfied by the tuple AND at least one filter
/// pins the owning tenant; the scope admits iff *some* constraint admits.
///
/// * **Tenant isolation is mandatory.** A constraint that does not pin the
///   record's `OWNER_TENANT_ID` (via `Eq` / `In`) never admits, even if its
///   other predicates match — `usage_record` grants are tenant-scoped, so an
///   admitting path must always name the tenant. UUID-as-`String` values are
///   accepted, mirroring [`AccessScope::contains_uuid`] / [`scope_value_to_ast`].
/// * **Every other predicate also constrains.** A constraint that additionally
///   narrows by `gts_type_id` / `resource_type` / `resource_id` / `subject`
///   only admits a record whose tuple satisfies those filters too. DESIGN
///   §3.9.6: a write needs "a returned scope that admits the full attribution
///   tuple — tenant, resource, referenced GTS type, and subject where
///   supplied". This gate is where the referenced-GTS-type half is enforced;
///   the `OData` projection deliberately does not honour the same property (see
///   [`pep_property_to_field`], including why a meter-narrowed grant admits a
///   measurement here and is refused there).
/// * **Unevaluable filters fail closed.** A tree predicate
///   ([`ScopeFilter::InGroup`] / [`ScopeFilter::InGroupSubtree`] /
///   [`ScopeFilter::InTenantSubtree`]) or a filter over a property outside the
///   [`usage_record::RESOURCE`] attribute set cannot be evaluated against a
///   flat per-record tuple, so the enclosing constraint cannot admit. Other
///   (OR-ed) constraints may still admit; dropping an unevaluable disjunct only
///   narrows access, never widens it.
/// * **An unconstrained (`allow_all`) scope is denied.** Under
///   `require_constraints(true)` a legitimate permit always carries the
///   `OWNER_TENANT_ID In [..]` narrowing (admin included, as `In [all
///   tenants]`), so `allow_all` only arises from a degenerate empty-predicate
///   permit and [`AccessScope::is_unconstrained`] short-circuits to a denial.
///   [`scope_to_odata_filter`] fails closed on the same shape, so both gates
///   share one posture.
///
/// # Return value
///
/// `Ok(())` when some constraint admits. Otherwise `Err` naming **which**
/// attribution attribute rejected — see [`TupleRejection`]. A `Result` rather
/// than a `bool` so the denial can say what failed instead of blaming the
/// tenant for every narrowing predicate the scope carries.
///
/// It reports the **most specific** failure across the OR-ed constraints: if
/// any disjunct pinned the record's tenant and still rejected on another
/// attribute, that attribute is named, because "not authorized for owning
/// tenant" is actively wrong there. A disjunct that never pinned the tenant
/// contributes `Tenant` whatever else it rejected on, so a cross-tenant record
/// is reported as cross-tenant — naming the other attribute would imply the
/// grant covers the record's tenant when it does not.
///
/// **Does not realize `cpt-cf-usage-collector-dod-write-scope-admission`
/// whole.** That `DoD` requires "the action MUST follow the route the caller
/// used, never the entry's covered period or kind", while
/// `covered_period.rs`'s `ingestion_action` picks a backfill entry's PEP action
/// from that entry's own `window_end` — what the sentence forbids. This
/// function does check the tenant/resource/type/subject dimensions the `DoD`
/// also requires, so `algo-write-scope-admission` is unaffected; only the
/// action-follows-route clause is left unticked.
// @cpt-algo:cpt-cf-usage-collector-algo-write-scope-admission:p1
fn scope_admits_attribution_tuple(
    scope: &AccessScope,
    key: &AttributionTupleKey,
) -> Result<(), TupleRejection> {
    if scope.is_unconstrained() {
        return Err(TupleRejection::Tenant);
    }
    let mut narrowed: Option<String> = None;
    for constraint in scope.constraints() {
        match constraint_verdict(constraint, key) {
            Ok(()) => return Ok(()),
            Err(TupleRejection::Tenant) => {}
            Err(TupleRejection::Property(property)) => {
                narrowed.get_or_insert(property);
            }
        }
    }
    Err(narrowed.map_or(TupleRejection::Tenant, TupleRejection::Property))
}

/// Which attribution attribute a granted scope rejected the record on.
///
/// The gate's verdict is a yes/no, but its *denial message* is not: an
/// operator reading it has to know which policy predicate to look at, and any
/// narrowing predicate a constraint carries can reject here.
#[domain_model]
#[derive(Debug, PartialEq, Eq)]
enum TupleRejection {
    /// The rejection is on the tenant dimension: no access path both pinned
    /// the record's owning tenant and admitted it. Covers a constraint that
    /// pinned no tenant at all (an empty / unconstrained grant included), one
    /// whose `OWNER_TENANT_ID` filter rejected, and one whose only rejecting
    /// predicate was over `OWNER_TENANT_ID` — an unevaluable tenant subtree,
    /// say, which never pins.
    Tenant,
    /// Some access path **pinned the record's owning tenant** and then
    /// narrowed by this attribution property, which the record does not
    /// satisfy. Carries the property name as the PDP spelled it, so an
    /// unevaluable (unknown) predicate names itself too. Only ever produced by
    /// a constraint that satisfied the tenant, which is what makes the
    /// operator-facing wording true rather than merely specific.
    Property(String),
}

/// Whether a single PDP [`ScopeConstraint`] (an AND of filters) admits the
/// record's attribution tuple, and on what it rejected otherwise. Requires the
/// owning tenant to be pinned and every filter satisfied; an empty constraint
/// (an allow-all disjunct) and any unevaluable filter both fail closed. See
/// [`scope_admits_attribution_tuple`].
///
/// **The loop runs to completion rather than returning on the first
/// rejection**, which is what makes the classification independent of the order
/// the PDP emitted its predicates in. Classifying on the first rejecting filter
/// does not: `[Eq(resource_id, "granted"), Eq(owner_tenant_id, T1)]` against a
/// `T2` record carrying `"other"` would answer `Property` one way round and
/// `Tenant` the other, and the `Property` answer tells an operator the grant
/// covers `T2`. The admit/deny verdict is unaffected: a constraint admits iff
/// nothing rejected **and** the tenant was pinned.
// @cpt-dod:cpt-cf-usage-collector-dod-tenant-scope-independence:p1
fn constraint_verdict(
    constraint: &ScopeConstraint,
    key: &AttributionTupleKey,
) -> Result<(), TupleRejection> {
    let mut tenant_pinned = false;
    let mut rejected = false;
    let mut rejected_property: Option<String> = None;
    for filter in constraint.filters() {
        match evaluate_filter(filter, key) {
            FilterOutcome::TenantSatisfied => tenant_pinned = true,
            FilterOutcome::Satisfied => {}
            FilterOutcome::Rejected => {
                rejected = true;
                // Classify by the rejecting filter's *property*, not through
                // `is_owner_tenant_filter`: that classifier answers "does this
                // pin the tenant", and a tree predicate over `owner_tenant_id`
                // deliberately does not pin, yet it is still the tenant
                // dimension that refused.
                if filter.property() != pep_properties::OWNER_TENANT_ID {
                    rejected_property.get_or_insert_with(|| filter.property().to_owned());
                }
            }
        }
    }
    // An empty constraint matches every row (an allow-all disjunct); under
    // require_constraints(true) we refuse to honour it, so the absence of a
    // satisfied tenant filter is itself a denial.
    if tenant_pinned && !rejected {
        return Ok(());
    }
    // A constraint that never pinned the record's tenant is a tenant rejection
    // whatever else it also rejected on. `rejected_property` can still be
    // `None` with the tenant pinned, when one tenant filter satisfied and
    // another rejected; that is the tenant's too.
    Err(if tenant_pinned {
        rejected_property.map_or(TupleRejection::Tenant, TupleRejection::Property)
    } else {
        TupleRejection::Tenant
    })
}

/// Outcome of checking one PDP filter against the record's attribution tuple.
#[domain_model]
enum FilterOutcome {
    /// An `OWNER_TENANT_ID` filter the record's owning tenant satisfies.
    TenantSatisfied,
    /// A non-tenant, gate-understood filter the record satisfies.
    Satisfied,
    /// The record does not satisfy the filter, or the filter cannot be
    /// evaluated against a flat per-record tuple (tree predicate, unknown
    /// property, or un-comparable value). Either way the enclosing constraint
    /// cannot admit — fail closed.
    Rejected,
}

/// Evaluate one [`ScopeFilter`] against the record's tuple. The property →
/// value mapping mirrors the `resource_property` writes in
/// [`authorize_attribution_tuple`]; keep the two in sync.
fn evaluate_filter(filter: &ScopeFilter, key: &AttributionTupleKey) -> FilterOutcome {
    let matched = match filter {
        ScopeFilter::Eq(eq) => {
            let property = eq.property();
            // Both are keyed on the same property set; either miss means the
            // property is outside the gear's vocabulary -> fail closed.
            let (Some(field), Some(value)) = (pep_field(property), tuple_value_for(property, key))
            else {
                return rejected_unknown_property(property);
            };
            value_matches(value, field.kind, eq.value())
        }
        ScopeFilter::In(in_filter) => {
            let property = in_filter.property();
            let (Some(field), Some(value)) = (pep_field(property), tuple_value_for(property, key))
            else {
                return rejected_unknown_property(property);
            };
            in_filter
                .values()
                .iter()
                .any(|v| value_matches(value, field.kind, v))
        }
        // Tree predicates, and any variant added later: a predicate this build
        // cannot evaluate is a restriction it cannot honour, so it must reject
        // rather than skip.
        _ => {
            tracing::warn!(
                target: "authz",
                property = %filter.property(),
                "PDP returned an unsupported tree predicate on the per-record \
                 usage_record gate: usage_records is a flat resource with no \
                 resource-group or tenant-closure membership; failing closed"
            );
            return FilterOutcome::Rejected;
        }
    };
    if !matched {
        return FilterOutcome::Rejected;
    }
    // Consult the shared classifier so the per-record gate and the LIST
    // projection agree on what counts as tenant narrowing.
    if is_owner_tenant_filter(filter) {
        FilterOutcome::TenantSatisfied
    } else {
        FilterOutcome::Satisfied
    }
}

fn rejected_unknown_property(property: &str) -> FilterOutcome {
    tracing::warn!(
        target: "authz",
        property = %property,
        "PDP returned a constraint over an unknown property on the per-record \
         usage_record gate: refuse to admit under an unrecognised attribute"
    );
    FilterOutcome::Rejected
}

/// The record's value for a PEP property, in a form comparable to a
/// [`ScopeValue`]. `Absent` denotes an optional attribute the record does not
/// carry, so a constraint filtering on it cannot be satisfied.
#[domain_model]
#[derive(Clone, Copy)]
enum TupleValue<'a> {
    Uuid(Uuid),
    Str(&'a str),
    Absent,
}

/// Resolve a PEP property to the record's tuple value, or `None` when the
/// property is outside the [`usage_record::RESOURCE`] attribute set. Mirrors
/// the `resource_property` writes in [`authorize_attribution_tuple`].
fn tuple_value_for<'a>(property: &str, key: &'a AttributionTupleKey) -> Option<TupleValue<'a>> {
    if property == pep_properties::OWNER_TENANT_ID {
        return Some(TupleValue::Uuid(key.tenant_id));
    }
    if property == pep_properties::OWNER_ID {
        return Some(
            key.subject_id
                .as_deref()
                .map_or(TupleValue::Absent, TupleValue::Str),
        );
    }
    match property {
        // @cpt-begin:cpt-cf-usage-collector-algo-write-scope-admission:p1:inst-writeadm-type
        usage_record::PROP_GTS_TYPE_ID => Some(TupleValue::Str(&key.gts_type_id)),
        // @cpt-end:cpt-cf-usage-collector-algo-write-scope-admission:p1:inst-writeadm-type
        usage_record::PROP_RESOURCE_TYPE => Some(TupleValue::Str(&key.resource_type)),
        usage_record::PROP_RESOURCE_ID => Some(TupleValue::Str(&key.resource_id)),
        usage_record::PROP_SUBJECT_TYPE => Some(
            key.subject_type
                .as_deref()
                .map_or(TupleValue::Absent, TupleValue::Str),
        ),
        _ => None,
    }
}

/// Whether the record's `tuple_value` (already in its native kind) equals the
/// PDP `scope_value` once coerced to the field's `kind`. Routes the coercion
/// through the shared [`coerce_scope_value`] so the per-record gate and the
/// LIST projection ([`scope_value_to_ast`]) cannot disagree on value typing —
/// a UUID-shaped `resource_id` is accepted on both, a value that does not
/// coerce to the field's kind (or an absent optional attribute) fails closed.
fn value_matches(tuple_value: TupleValue, kind: OdataFieldKind, scope_value: &ScopeValue) -> bool {
    match (tuple_value, coerce_scope_value(kind, scope_value)) {
        (TupleValue::Uuid(u), Some(CanonicalValue::Uuid(v))) => u == v,
        (TupleValue::Str(s), Some(CanonicalValue::Str(v))) => s == v.as_str(),
        _ => false,
    }
}

/// Authorize a `list_usage_records` request and return the compiled
/// [`AccessScope`] for downstream `OData` composition.
///
/// `require_constraints(true)` so the PDP MUST return row-scope narrowing for
/// a tenant-scoped caller rather than short-circuiting to an unconstrained
/// `AccessScope::allow_all`. The authz-resolver materializes the caller's
/// tenant closure into a flat `OWNER_TENANT_ID In [..]` constraint —
/// `usage_record` does not advertise `Capability::TenantHierarchy`, so the
/// closure is expanded eagerly rather than pushed down as an `InTenantSubtree`
/// predicate this flat resource cannot consume. A platform-admin (`Global`
/// scope) still resolves to a constraint over the full tenant set, so requiring
/// constraints does not deny admin. The compiled scope is projected through
/// [`scope_to_odata_filter`] at the call site and AND-merged into the user's
/// filter before plugin dispatch (see
/// [`crate::domain::service::Service::list_usage_records`]).
///
/// The request carries no per-record attribution attributes (LIST is pre-row),
/// so the composed PEP request is action+resource-type only.
///
/// # Errors
///
/// * [`DomainError::AuthorizationDenied`] when the PDP denies or returns
///   an uncompilable constraint shape.
/// * [`DomainError::AuthorizationUnavailable`] when the PDP transport
///   fails.
// @cpt-algo:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1
pub(crate) async fn authorize_list_usage_records(
    enforcer: &PolicyEnforcer,
    metrics: &dyn UsageCollectorMetrics,
    op: PdpOp,
    ctx: &SecurityContext,
) -> Result<AccessScope, DomainError> {
    pdp_scope_with(
        enforcer,
        metrics,
        op,
        ctx,
        &usage_record::RESOURCE,
        usage_record::actions::LIST,
        None,
        &AccessRequest::new().require_constraints(true),
        // LIST authorizes in two stages like the per-record path: the PDP
        // permit must ALSO yield a projectable row scope or it fails closed.
        // Running `scope_to_odata_filter` as the gate classifies the effective
        // decision for `uc_authz_decisions_total`. The projected `Expr` is
        // discarded here and recomputed by `compose_query_with_scope` at the
        // call site; it is the same pure projection, so the decision recorded
        // here and the filter composed there cannot disagree.
        |scope| scope_to_odata_filter(&scope).map(|_| scope),
    )
    .await
}

/// Authorize a `get_usage_record` (point lookup) request and return the
/// **already-projected** `OData` filter expression the plugin must apply.
///
/// Mirrors [`authorize_list_usage_records`] in every respect — pre-row PEP
/// request, `require_constraints(true)`, the same [`scope_to_odata_filter`]
/// projectability gate — except the PEP `action`:
/// `usage_record::actions::GET`, not `LIST`. The two verbs share the same
/// resource and attribute set but are distinct PEP actions, so a policy
/// permitting one need not permit the other.
///
/// The gate's success value is the projected [`ast::Expr`] itself, not the
/// [`AccessScope`]: the point lookup carries no caller-supplied filter to AND
/// it with (there is no
/// [`compose_query_with_scope`](super::query::compose_query_with_scope)
/// analogue here), so the projected scope *is* the whole filter handed to
/// `UsageCollectorPluginV1::get_usage_record`.
///
/// # Errors
///
/// * [`DomainError::AuthorizationDenied`] when the PDP denies, or returns
///   an unconstrained / deny-all / un-projectable (tree predicate, unknown
///   property, not tenant-pinned) constraint shape — see
///   [`scope_to_odata_filter`] for the full fail-closed enumeration.
/// * [`DomainError::AuthorizationUnavailable`] when the PDP transport
///   fails.
pub(crate) async fn authorize_get_usage_record_scope(
    enforcer: &PolicyEnforcer,
    metrics: &dyn UsageCollectorMetrics,
    op: PdpOp,
    ctx: &SecurityContext,
) -> Result<ast::Expr, DomainError> {
    pdp_scope_with(
        enforcer,
        metrics,
        op,
        ctx,
        &usage_record::RESOURCE,
        usage_record::actions::GET,
        None,
        &AccessRequest::new().require_constraints(true),
        |scope| scope_to_odata_filter(&scope),
    )
    .await
}

/// Authorize a feed read and return the **already-projected** `OData` filter
/// expression the plugin applies as its scope.
///
/// Pre-row, like [`authorize_list_usage_records`]: the PEP request carries no
/// per-record attribution attributes, because a feed page is not a record, and
/// the PDP returns row narrowing through the [`AccessScope`] constraints.
///
/// It returns the projected [`ast::Expr`] rather than the [`AccessScope`],
/// matching [`authorize_get_usage_record_scope`]: the feed admits no caller
/// order and no caller filter at all, and
/// `UsageCollectorPluginV1::read_feed_page` takes a `scope: &ast::Expr`
/// directly.
///
/// The subscription is **not** part of this: it travels beside the scope as its
/// own SPI argument, never inside it — and it remains the only thing that
/// narrows this path by type. A PDP constraint naming
/// [`usage_record::PROP_GTS_TYPE_ID`] does not: [`pep_property_to_field`]
/// refuses it by name, failing the whole projection closed including any
/// sibling disjunct, because [`scope_to_odata_filter`]'s per-constraint loop
/// propagates with `?`. That refusal binds the read paths and the ingestion
/// path's invalidation-target lookup alike; [`pep_property_to_field`]
/// enumerates them.
///
/// # Errors
///
/// * [`DomainError::AuthorizationDenied`] when the PDP denies, or returns an
///   unconstrained / deny-all / un-projectable constraint shape — see
///   [`scope_to_odata_filter`].
/// * [`DomainError::AuthorizationUnavailable`] when the PDP transport fails.
// @cpt-algo:cpt-cf-usage-collector-algo-feed-scope-change:p1
// @cpt-dod:cpt-cf-usage-collector-dod-feed-scope-change-asymmetry:p1
// @cpt-flow:cpt-cf-usage-collector-flow-feed-bootstrap-after-widening:p1
// @cpt-flow:cpt-cf-usage-collector-flow-authorize-feed:p1
// @cpt-dod:cpt-cf-usage-collector-dod-feed-scope-per-request:p1
pub(crate) async fn authorize_read_usage_feed(
    enforcer: &PolicyEnforcer,
    metrics: &dyn UsageCollectorMetrics,
    op: PdpOp,
    ctx: &SecurityContext,
) -> Result<ast::Expr, DomainError> {
    pdp_scope_with(
        enforcer,
        metrics,
        op,
        ctx,
        &usage_record::RESOURCE,
        usage_record::actions::READ_FEED,
        None,
        &AccessRequest::new().require_constraints(true),
        |scope| scope_to_odata_filter(&scope),
    )
    .await
}

/// Authorize a reconciliation read and compile the PDP scope it runs under.
///
/// Like [`authorize_list_usage_records`] and [`authorize_read_usage_feed`] it
/// authorizes **pre-row**: the PDP returns row narrowing through the compiled
/// scope, which the Query Gateway passes to the plugin as a separate argument
/// rather than folding into the requested tenant and type.
///
/// `require_constraints(true)`, so a permit carrying no constraints is a
/// degenerate empty-predicate grant and is denied rather than allowed to report
/// on every tenant — which `dod-reconciliation-operator-surface` requires.
///
/// # Errors
///
/// Every error [`scope_to_odata_filter`] documents, plus a PDP denial.
pub(crate) async fn authorize_get_reconciliation_metadata(
    enforcer: &PolicyEnforcer,
    metrics: &dyn UsageCollectorMetrics,
    op: PdpOp,
    ctx: &SecurityContext,
) -> Result<ast::Expr, DomainError> {
    pdp_scope_with(
        enforcer,
        metrics,
        op,
        ctx,
        &usage_record::RESOURCE,
        usage_record::actions::RECONCILE,
        None,
        &AccessRequest::new().require_constraints(true),
        |scope| scope_to_odata_filter(&scope),
    )
    .await
}

/// Project an [`AccessScope`] into an `OData` filter expression over the
/// `UsageRecord` raw-read filter surface.
///
/// Constraints are OR-ed at the [`AccessScope`] level (one constraint per
/// independent access path) and filters within a constraint are AND-ed —
/// see [`AccessScope`] docs. The returned expression mirrors that shape:
/// `(f1 and f2 and ...) or (g1 and g2 and ...)`. PEP property names are
/// translated to the `OData` wire fields declared on
/// [`usage_collector_sdk::UsageRecordFilterField`]:
///
/// | PEP property                          | `OData` field   | Value kind |
/// |---------------------------------------|-----------------|------------|
/// | `pep_properties::OWNER_TENANT_ID`     | `tenant_id`     | UUID       |
/// | `pep_properties::OWNER_ID`            | `subject_id`    | string     |
/// | `usage_record::PROP_RESOURCE_TYPE`    | `resource_type` | string     |
/// | `usage_record::PROP_RESOURCE_ID`      | `resource_id`   | string     |
/// | `usage_record::PROP_SUBJECT_TYPE`     | `subject_type`  | string     |
///
/// [`usage_record::PROP_GTS_TYPE_ID`] is advertised on the resource but has
/// **no row here**: [`pep_property_to_field`] refuses it, so a constraint
/// naming it denies rather than projecting. That refusal binds every caller of
/// this function, which is **not** only the read paths — `project_pdp_decisions`
/// in `domain/service.rs` projects the ingestion `create` permit through here
/// too. See [`pep_property_to_field`] for why, and DESIGN §3.9.6's table row.
///
/// Always projects to a narrowing predicate (`Ok(expr)`) or fails closed. There
/// is **no** "no row narrowing" pass-through: under `require_constraints(true)`
/// a legitimate permit always carries `OWNER_TENANT_ID In [..]` narrowing
/// (admin included, as `In [all tenants]`), so an unconstrained /
/// empty-constraint scope is a degenerate empty-predicate permit and is denied
/// rather than allowed to leak every tenant's rows — mirroring the per-record
/// [`scope_admits_attribution_tuple`] gate.
///
/// # Errors
///
/// * [`DomainError::AuthorizationDenied`] when the scope is unconstrained
///   ([`AccessScope::is_unconstrained`]) or carries an empty (filter-less)
///   constraint disjunct — both match every row, so collapsing to "no row
///   narrowing" would breach tenant isolation. Fail closed instead, per
///   [`cpt-cf-usage-collector-algo-pdp-scope-evaluation`].
/// * [`DomainError::AuthorizationDenied`] when a constraint is non-empty but
///   carries no `OWNER_TENANT_ID` `Eq`/`In` filter — a constraint narrowing
///   only by some other property (e.g. `resource_type`) would AND into the
///   user query as a cross-tenant predicate, so it is denied. This mirrors the
///   per-record gate's tenant-pinning requirement (see
///   [`is_owner_tenant_filter`]).
/// * [`DomainError::AuthorizationDenied`] when the scope is deny-all
///   ([`AccessScope::is_deny_all`]) — the PDP explicitly authorized no
///   rows, which is observationally indistinguishable from a deny on
///   this surface.
/// * [`DomainError::AuthorizationDenied`] when a constraint carries a tree
///   predicate ([`ScopeFilter::InGroup`] / [`ScopeFilter::InGroupSubtree`] /
///   [`ScopeFilter::InTenantSubtree`]) — `usage_records` is a flat resource
///   without resource-group or tenant-closure membership tables, so a tree
///   predicate cannot be compiled against this plugin's storage.
/// * [`DomainError::AuthorizationDenied`] when a constraint names a PEP
///   property outside the [`usage_record::RESOURCE`] attribute set — same
///   fail-closed rationale.
/// * [`DomainError::AuthorizationDenied`] when a constraint names
///   [`usage_record::PROP_GTS_TYPE_ID`], which is *inside* that set but
///   reserved on the projection — a distinct reason from the one above, because
///   its cause and its fix differ. One bad disjunct fails the whole scope, and
///   the denial reaches the invalidation-target lookup as well as the reads.
// @cpt-algo:cpt-cf-usage-collector-algo-pdp-scope-evaluation:p1
pub(crate) fn scope_to_odata_filter(scope: &AccessScope) -> Result<ast::Expr, DomainError> {
    if scope.is_unconstrained() {
        tracing::warn!(
            target: "authz",
            "PDP returned an unconstrained (allow_all) scope on the usage_record \
             query path under require_constraints(true); failing closed"
        );
        return Err(DomainError::AuthorizationDenied {
            reason: Some("PDP returned an unconstrained scope".to_owned()),
        });
    }
    if scope.is_deny_all() {
        tracing::warn!(
            target: "authz",
            "PDP returned a deny-all scope on the usage_record query path"
        );
        return Err(DomainError::AuthorizationDenied {
            reason: Some("PDP returned a deny-all scope".to_owned()),
        });
    }

    let mut disjunction: Option<ast::Expr> = None;
    for constraint in scope.constraints() {
        let constraint_expr = constraint_to_odata_conjunction(constraint)?;
        disjunction = Some(match disjunction {
            None => constraint_expr,
            Some(acc) => acc.or(constraint_expr),
        });
    }
    // Unreachable in practice — the `is_unconstrained` / `is_deny_all` guards
    // above leave `constraints()` non-empty, and a non-empty constraint always
    // yields a `Some` disjunct (an empty one fails closed). Deny anyway so the
    // projection can never silently emit "no row narrowing".
    disjunction.ok_or_else(|| DomainError::AuthorizationDenied {
        reason: Some("PDP returned a scope with no usable constraints".to_owned()),
    })
}

/// Project one [`ScopeConstraint`] (an AND of filters) into an `OData`
/// conjunction, or fail closed.
///
/// A constraint fails closed under `require_constraints(true)` two ways:
///
/// * **Empty (filter-less)** — matches every row, an allow-all disjunct.
/// * **Not tenant-pinned** — carries no `OWNER_TENANT_ID` `Eq`/`In` filter.
///   A constraint narrowing only by, say, `resource_type` would AND into the
///   user query as a *cross-tenant* predicate.
///
/// Either would collapse the projection to "no tenant narrowing" and leak every
/// tenant's records, so both are denied — mirroring [`constraint_verdict`].
/// Both consult the shared [`is_owner_tenant_filter`] so neither can drift on
/// what counts as tenant narrowing.
fn constraint_to_odata_conjunction(constraint: &ScopeConstraint) -> Result<ast::Expr, DomainError> {
    let mut conjunction: Option<ast::Expr> = None;
    let mut tenant_pinned = false;
    for filter in constraint.filters() {
        tenant_pinned |= is_owner_tenant_filter(filter);
        let predicate = scope_filter_to_expr(filter)?;
        conjunction = Some(match conjunction {
            None => predicate,
            Some(acc) => acc.and(predicate),
        });
    }
    let Some(conjunction) = conjunction else {
        tracing::warn!(
            target: "authz",
            "PDP returned an empty (allow-all) constraint on the usage_record \
             query path under require_constraints(true); failing closed"
        );
        return Err(DomainError::AuthorizationDenied {
            reason: Some("PDP returned an empty constraint".to_owned()),
        });
    };
    if !tenant_pinned {
        tracing::warn!(
            target: "authz",
            "PDP returned a usage_record LIST constraint without OWNER_TENANT_ID \
             narrowing under require_constraints(true); failing closed"
        );
        return Err(DomainError::AuthorizationDenied {
            reason: Some("PDP returned a constraint without tenant narrowing".to_owned()),
        });
    }
    Ok(conjunction)
}

/// Whether `filter` pins the owning tenant — an `Eq`/`In` on
/// [`pep_properties::OWNER_TENANT_ID`]. Tree predicates over the tenant
/// property never pin (they fail closed upstream as unsupported on a flat
/// resource). Shared by [`evaluate_filter`] and
/// [`constraint_to_odata_conjunction`] so neither can drift.
fn is_owner_tenant_filter(filter: &ScopeFilter) -> bool {
    match filter {
        ScopeFilter::Eq(eq) => eq.property() == pep_properties::OWNER_TENANT_ID,
        ScopeFilter::In(in_filter) => in_filter.property() == pep_properties::OWNER_TENANT_ID,
        // Only a flat predicate on `owner_tenant_id` counts as tenant
        // narrowing; a tree predicate, or one from a newer build, does not.
        _ => false,
    }
}

/// Map a single [`ScopeFilter`] to an `OData` [`ast::Expr`].
fn scope_filter_to_expr(filter: &ScopeFilter) -> Result<ast::Expr, DomainError> {
    match filter {
        ScopeFilter::Eq(eq) => {
            let field = pep_property_to_field(eq.property())?;
            let value = scope_value_to_ast(field, eq.value())?;
            Ok(ast::Expr::Compare(
                Box::new(ast::Expr::Identifier(field.name.to_owned())),
                ast::CompareOperator::Eq,
                Box::new(ast::Expr::Value(value)),
            ))
        }
        ScopeFilter::In(in_filter) => {
            let field = pep_property_to_field(in_filter.property())?;
            let values: Vec<ast::Expr> = in_filter
                .values()
                .iter()
                .map(|v| scope_value_to_ast(field, v).map(ast::Expr::Value))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ast::Expr::In(
                Box::new(ast::Expr::Identifier(field.name.to_owned())),
                values,
            ))
        }
        // Tree predicates, and any variant added later: the projection cannot
        // express the restriction, so it denies instead of emitting a filter
        // that leaves the restriction out.
        _ => {
            tracing::warn!(
                target: "authz",
                property = %filter.property(),
                "PDP returned an unsupported tree predicate: usage_records is a \
                 flat resource with no resource-group or tenant-closure membership"
            );
            Err(DomainError::AuthorizationDenied {
                reason: Some(format!(
                    "PDP returned an unsupported tree predicate on property `{}`: \
                     usage_records is a flat resource with no resource-group or \
                     tenant-closure membership",
                    filter.property()
                )),
            })
        }
    }
}

/// Wire description of a PDP property's `OData` projection.
#[domain_model]
#[derive(Clone, Copy)]
struct OdataField {
    /// `OData` identifier visible on the [`usage_collector_sdk::UsageRecordFilterField`] surface.
    name: &'static str,
    /// Expected [`ScopeValue`] variant. Anything else lifts to a fail-closed deny.
    kind: OdataFieldKind,
}

#[domain_model]
#[derive(Clone, Copy)]
enum OdataFieldKind {
    Uuid,
    String,
}

/// THE single registry of the `usage_record` PEP property set: each recognized
/// property's `OData` wire field name and canonical value [`OdataFieldKind`].
///
/// Both authz gates resolve properties through this one function — the
/// per-record gate ([`evaluate_filter`]) reads the `kind`, the `OData`
/// projection ([`pep_property_to_field`]) reads the `name` and `kind` — so the
/// recognized property set and its typing **cannot drift**. Returns `None` for
/// a property outside the [`usage_record::RESOURCE`] attribute set; each caller
/// lifts that into its own fail-closed denial.
///
/// What this function recognizes is exactly [`usage_record::RESOURCE`]'s
/// advertised set; what the `OData` projection *honours* is a strict subset,
/// because [`pep_property_to_field`] refuses
/// [`usage_record::PROP_GTS_TYPE_ID`] before consulting this registry. That
/// property is therefore scopable on the attribution tuple and reserved on the
/// projection — which is not the same as "reserved on reads". The reason, and
/// why making the two symmetric is the one unsafe edit here, is on
/// [`pep_property_to_field`].
fn pep_field(property: &str) -> Option<OdataField> {
    if property == pep_properties::OWNER_TENANT_ID {
        return Some(OdataField {
            name: "tenant_id",
            kind: OdataFieldKind::Uuid,
        });
    }
    if property == pep_properties::OWNER_ID {
        return Some(OdataField {
            name: "subject_id",
            kind: OdataFieldKind::String,
        });
    }
    match property {
        // **Only the `kind` of this arm is ever used**, by the per-record
        // attribution gate (`evaluate_filter`). The `OData` projection never
        // reaches here for this property, because `pep_property_to_field`
        // refuses `gts_type_id` first — a deliberate asymmetry argued there;
        // read it before making these two agree. The `name` is carried only so
        // the struct stays coherent.
        //
        // `OdataFieldKind::String` is the meter's own kind:
        // `UsageRecord::gts_type_id` is a `MeterTypeId` over the GTS type id
        // string, and the plugin's column is `text`.
        usage_record::PROP_GTS_TYPE_ID => Some(OdataField {
            name: "gts_type_id",
            kind: OdataFieldKind::String,
        }),
        usage_record::PROP_RESOURCE_TYPE => Some(OdataField {
            name: "resource_type",
            kind: OdataFieldKind::String,
        }),
        usage_record::PROP_RESOURCE_ID => Some(OdataField {
            name: "resource_id",
            kind: OdataFieldKind::String,
        }),
        usage_record::PROP_SUBJECT_TYPE => Some(OdataField {
            name: "subject_type",
            kind: OdataFieldKind::String,
        }),
        _ => None,
    }
}

/// A PDP [`ScopeValue`] coerced to the canonical comparable form for a field's
/// [`OdataFieldKind`]. Produced by [`coerce_scope_value`] and consumed by both
/// gates — the per-record gate compares it against the record's value, the
/// LIST projection lowers it to an [`ast::Value`].
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
enum CanonicalValue {
    Uuid(Uuid),
    Str(String),
}

/// THE single `ScopeValue` coercion policy shared by both authz gates.
///
/// * A **UUID** field accepts a `Uuid` or a UUID-shaped `String`, mirroring
///   [`ScopeValue::as_uuid`] / [`AccessScope::contains_uuid`].
/// * A **String** field accepts a `String` or a `Uuid` rendered to its
///   canonical string form, so a UUID-shaped `resource_id` matches regardless
///   of how the PEP compiler typed it.
/// * Anything else is a type mismatch and yields `None`; both gates then fail
///   closed.
///
/// Keeping this in one place is what stops the per-record gate and the
/// projection from disagreeing on value typing.
fn coerce_scope_value(kind: OdataFieldKind, value: &ScopeValue) -> Option<CanonicalValue> {
    match kind {
        OdataFieldKind::Uuid => value.as_uuid().map(CanonicalValue::Uuid),
        OdataFieldKind::String => match value {
            ScopeValue::String(s) => Some(CanonicalValue::Str(s.clone())),
            ScopeValue::Uuid(u) => Some(CanonicalValue::Str(u.to_string())),
            ScopeValue::Int(_) | ScopeValue::Bool(_) => None,
        },
    }
}

/// Projection-side property resolution: [`pep_field`] lifted into the
/// fail-closed [`DomainError::AuthorizationDenied`]
/// [`scope_to_odata_filter`] surfaces for an attribute outside the
/// [`usage_record::RESOURCE`] set — **plus one advertised property it refuses
/// outright.**
///
/// # Why this is not symmetric with [`pep_field`], and must not be made so
///
/// [`usage_record::PROP_GTS_TYPE_ID`] is advertised on the resource and
/// recognized by [`pep_field`], yet denied here, because the two callers of
/// [`pep_field`] consume different halves of its answer: the per-record gate
/// ([`evaluate_filter`]) reads only `field.kind`, to type a comparison it
/// performs in the gear, while this projection reads `field.name` and that
/// string becomes an [`ast::Expr::Identifier`] in the filter pushed to the
/// storage plugin.
///
/// The plugin refuses that identifier. Its `record_column`
/// (`query/translate.rs`) is a closed match whose own doc calls it the security
/// boundary — only the identifiers it names can ever reach the SQL string, and
/// `gts_type_id` is intentionally absent, being a typed parameter on the SPI
/// rather than a `$filter` field. The column exists; the exclusion is
/// deliberate, and
/// `translate_tests::a_real_column_that_is_not_a_filter_field_does_not_resolve`
/// asserts it. An unmapped field is a translate error, never an interpolated
/// column, so emitting the name here would convert a denial into a plugin
/// error.
///
/// # Which paths this denies — it is **not** "reads only"
///
/// Every caller that turns an [`AccessScope`] into an `OData` filter passes
/// through here:
///
/// * the read **PEP actions** — `list`, `get`, `read_feed`, `reconcile` —
///   whose gate is [`scope_to_odata_filter`] itself. That is fewer actions than
///   REST surfaces: [`authorize_list_usage_records`] authorizes under `LIST`
///   for both `Service::list_usage_records` and
///   `Service::query_aggregated_usage_records`, so the aggregate route is
///   affected exactly as `GET /usage-collector/v1/records` is;
/// * `Service::create_usage_records`' batch invalidation-target lookup — the
///   [`scope_to_odata_filter`] call in `domain/service.rs`'s
///   `project_pdp_decisions`, which compiles the group's `create` permit once
///   per group containing a withdrawal.
///
/// So a PDP grant narrowed by meter **admits a measurement and denies a
/// withdrawal**: [`scope_admits_attribution_tuple`] passes the measurement,
/// while the withdrawal's target lookup reaches this refusal and fails closed.
/// That is a real limitation of the feature, pinned by
/// `the_reserved_property_denies_a_withdrawal_under_a_meter_narrowed_grant`.
///
/// **Deleting this refusal to match [`pep_field`] is the one change that is not
/// fail-closed.** DESIGN §3.9.6's table marks the projection side reserved for
/// the same reason.
///
/// The reason text deliberately differs from the unknown-property one below: a
/// policy naming a property this gear never had and one naming a property it
/// reserves have different causes and different fixes. It also names the
/// withdrawal consequence, the half an operator cannot infer from "denied".
fn pep_property_to_field(property: &str) -> Result<OdataField, DomainError> {
    if property == usage_record::PROP_GTS_TYPE_ID {
        tracing::warn!(
            target: "authz",
            property = %property,
            "PDP returned a constraint over a property reserved on the \
             usage_record scope projection: the meter narrows the attribution \
             tuple on ingestion only, so every projection of this scope fails \
             closed rather than emitting an identifier the storage filter \
             vocabulary excludes. That binds the four read paths and the \
             invalidation-target lookup, so this grant can measure but cannot \
             withdraw"
        );
        return Err(DomainError::AuthorizationDenied {
            reason: Some(format!(
                "PDP returned a constraint over property `{property}`, which is \
                 reserved on the usage_record scope projection — the meter \
                 narrows the attribution tuple on ingestion only, and the \
                 storage filter vocabulary excludes the identifier by design. \
                 Every projection of this scope is refused: the read paths, and \
                 the invalidation-target lookup, so a grant narrowed by meter \
                 can measure but cannot withdraw"
            )),
        });
    }
    pep_field(property).ok_or_else(|| {
        tracing::warn!(
            target: "authz",
            property = %property,
            "PDP returned a constraint over an unknown property for the \
             usage_record resource: refuse to widen scope under an \
             unrecognised attribute"
        );
        DomainError::AuthorizationDenied {
            reason: Some(format!(
                "PDP returned a constraint over unknown property `{property}` for the \
                 usage_record resource — refuse to widen scope under an \
                 unrecognised attribute"
            )),
        }
    })
}

/// Lower a PDP [`ScopeValue`] to the `OData` value for `field`, sharing the
/// [`coerce_scope_value`] policy with the per-record gate so the two cannot
/// drift on value typing.
fn scope_value_to_ast(field: OdataField, value: &ScopeValue) -> Result<ast::Value, DomainError> {
    match coerce_scope_value(field.kind, value) {
        Some(CanonicalValue::Uuid(u)) => Ok(ast::Value::Uuid(u)),
        Some(CanonicalValue::Str(s)) => Ok(ast::Value::String(s)),
        None => {
            let expected = match field.kind {
                OdataFieldKind::Uuid => "UUID",
                OdataFieldKind::String => "string",
            };
            let actual = describe_scope_value(value);
            tracing::warn!(
                target: "authz",
                field = %field.name,
                expected = %expected,
                actual = %actual,
                "PDP returned a value that cannot be coerced to the constraint field's type"
            );
            Err(DomainError::AuthorizationDenied {
                reason: Some(format!(
                    "PDP returned a {actual} value for field `{}` typed as {expected}",
                    field.name,
                )),
            })
        }
    }
}

fn describe_scope_value(v: &ScopeValue) -> &'static str {
    match v {
        ScopeValue::Uuid(_) => "UUID",
        ScopeValue::String(_) => "string",
        ScopeValue::Int(_) => "integer",
        ScopeValue::Bool(_) => "boolean",
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "authz_tests.rs"]
mod authz_tests;
