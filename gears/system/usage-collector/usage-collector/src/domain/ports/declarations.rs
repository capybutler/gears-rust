//! Port for reading GTS type declarations.
//!
//! The Type Resolver depends on this one method rather than on the whole
//! `TypesRegistryClient` (14 methods and growing), so the resolver's caching
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
    ///   definite not-found answer. This is a resolvable fact, and the
    ///   resolver caches nothing for it.
    /// - [`DomainError::TypesRegistryUnavailable`] for any other failure.
    ///   The resolver may serve a stale cached declaration for this, because
    ///   it cannot tell an unavailable registry from a slow one.
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError>;
}
