//! Port for reading GTS type declarations.
//!
//! The Type Resolver depends on this one method rather than on the whole
//! `TypesRegistryClient` (13 methods and growing), so the resolver's caching
//! policy can be exercised in tests against a trivial fake and the real
//! registry adapter stays in `infra` — the same `domain/ports/` shape the
//! gear already uses for metrics.

use async_trait::async_trait;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;

/// Reads a meter's type declaration from its system of record.
///
/// Implemented in `infra` against `types-registry`'s `TypesRegistryClient`;
/// the domain-level Type Resolver (and its tests) depend only on this trait.
#[async_trait]
pub trait DeclarationSource: Send + Sync + 'static {
    /// Fetches the type schema for `id`.
    ///
    /// # Errors
    ///
    /// - [`DomainError::DeclarationNotFound`] (via
    ///   [`DomainError::declaration_not_found`]) when the registry gives a
    ///   definite not-found answer — a conclusive fact, not a possibly-stale
    ///   read — so the resolver caches nothing for it.
    /// - [`DomainError::TypesRegistryUnavailable`] for any other failure.
    ///   The resolver may serve a stale cached declaration for this, because
    ///   it cannot tell an unavailable registry from a slow one.
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError>;
}

/// [`DeclarationSource`] with no working backend.
///
/// Mirrors [`crate::domain::ports::metrics::NoopMetrics`]'s role for the
/// metrics port: a domain-owned placeholder for contexts that need *a*
/// implementation to construct a [`crate::domain::type_resolver::TypeResolver`]
/// but have no real adapter to give it. Every call reports the registry as
/// unavailable rather than panicking, so it fails safely rather than
/// violently if that ever stops being true.
///
/// Used only by [`crate::domain::service::Service::new`]'s convenience
/// default: that constructor cannot reach into `infra` to build the real
/// `types-registry` adapter without reintroducing the domain → infra edge
/// this port exists to prevent. Production bootstrap (`module.rs`) always
/// builds a genuine adapter-backed resolver instead (see
/// `crate::infra::types_registry_source::build_default_resolver`) and injects
/// it through [`crate::domain::service::Service::new_with_metrics`].
#[allow(dead_code)] // constructed only by Service::new's convenience default
pub struct UnavailableDeclarationSource;

#[async_trait]
impl DeclarationSource for UnavailableDeclarationSource {
    async fn fetch(&self, _id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        Err(DomainError::TypesRegistryUnavailable(
            "Service::new has no types-registry adapter wired".to_owned(),
        ))
    }
}
