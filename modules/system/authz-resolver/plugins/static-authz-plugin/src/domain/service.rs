// Updated: 2026-04-14 by Constructor Tech
//! Service implementation for the static `AuthZ` resolver plugin.

use authz_resolver_sdk::{
    Capability, Constraint, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
    InPredicate, InTenantSubtreePredicate, Predicate,
};
use modkit_macros::domain_model;
use modkit_security::pep_properties;
use uuid::Uuid;

/// Static `AuthZ` resolver service.
///
/// - Returns `decision: true` with an `in` predicate on `pep_properties::OWNER_TENANT_ID`
///   scoped to the context tenant from the request (for all operations including CREATE).
/// - Additionally emits parallel `InTenantSubtree(<prop>, tid)` constraints (one per
///   tenant-shaped supported property) when the PEP advertises
///   [`Capability::TenantHierarchy`]. This lets entities whose `Scopable` declaration
///   is `no_tenant, resource_col = "..."` (e.g. AM's `tenants`) bind via
///   `InTenantSubtree(RESOURCE_ID, tid)`, and entities declared `tenant_col = "..."`
///   that opt-in to subtree access (e.g. AM's `tenant_metadata` / `conversion_requests`)
///   bind via `InTenantSubtree(OWNER_TENANT_ID, tid)` -- without that addition the
///   `In(OWNER_TENANT_ID)` clamp restricts visibility to the caller's own tenant row,
///   hiding direct-child writes the test fixtures exercise.
/// - Constraints are OR-ed, and the `SecureORM` compiler drops any predicate whose
///   property doesn't resolve to a column on the entity being queried -- so the
///   addition is invisible to PEPs that don't advertise the capability and to entities
///   that don't expose the property.
/// - Denies access (`decision: false`) when no valid tenant can be resolved.
#[domain_model]
#[derive(Default)]
pub struct Service;

impl Service {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Evaluate an authorization request.
    #[must_use]
    #[allow(clippy::unused_self)] // &self reserved for future config/state
    pub fn evaluate(&self, request: &EvaluationRequest) -> EvaluationResponse {
        // Always scope to context tenant (all CRUD operations get constraints)
        let tenant_id = request
            .context
            .tenant_context
            .as_ref()
            .and_then(|t| t.root_id)
            .or_else(|| {
                // Fallback: extract tenant_id from subject properties
                request
                    .subject
                    .properties
                    .get("tenant_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            });

        let Some(tid) = tenant_id else {
            // No tenant resolvable from context or subject - deny access.
            return EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            };
        };

        if tid == Uuid::default() {
            // Nil UUID tenant - deny rather than grant unrestricted access.
            return EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            };
        }

        // Baseline OWNER_TENANT_ID clamp -- the universal shape every PEP can bind
        // when its entity declares `tenant_col`.
        let mut constraints = vec![Constraint {
            predicates: vec![Predicate::In(InPredicate::new(
                pep_properties::OWNER_TENANT_ID,
                [tid],
            ))],
        }];

        // Closes the gap from `cyberware-rust#1813` (plugin half) for the dev stack:
        // PEPs that advertise `Capability::TenantHierarchy` get an
        // `InTenantSubtree(<prop>, tid)` constraint for each
        // tenant-shaped property they declare as supported
        // (`OWNER_TENANT_ID` and `RESOURCE_ID`). The two predicates target
        // different entity shapes:
        //
        // * `InTenantSubtree(OWNER_TENANT_ID, tid)` binds against entities
        //   declared with `tenant_col` (e.g. AM's `tenant_metadata` /
        //   `conversion_requests`) and clamps to the caller's subtree --
        //   the contract the test fixtures exercise when a caller in the
        //   root tenant writes metadata on a direct child.
        // * `InTenantSubtree(RESOURCE_ID, tid)` binds against entities
        //   declared with `resource_col` (e.g. AM's `tenants` itself,
        //   `no_tenant`) and clamps via the resource id.
        //
        // The constraints are OR-ed, and the SecureORM compiler drops any
        // predicate whose property doesn't resolve to a column on the
        // entity being queried -- so the addition is invisible to PEPs
        // that don't advertise the capability and to entities that don't
        // expose the property. Modules that do not opt-in to
        // `Capability::TenantHierarchy` see the baseline shape unchanged.
        if advertises_tenant_hierarchy(request) {
            for prop in [pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID] {
                if supports_property(request, prop) {
                    constraints.push(Constraint {
                        predicates: vec![Predicate::InTenantSubtree(
                            InTenantSubtreePredicate::new(prop, tid),
                        )],
                    });
                }
            }
        }

        EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints,
                ..Default::default()
            },
        }
    }
}

fn advertises_tenant_hierarchy(request: &EvaluationRequest) -> bool {
    request
        .context
        .capabilities
        .iter()
        .any(|c| matches!(c, Capability::TenantHierarchy))
}

fn supports_property(request: &EvaluationRequest, property: &str) -> bool {
    request
        .context
        .supported_properties
        .iter()
        .any(|p| p == property)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
