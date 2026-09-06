//! `DeclarationSource` adapter over the `types-registry` SDK client.
//!
//! Resolving the client per call rather than holding it keeps the gear's
//! startup free of an eager `types-registry` dependency, matching how the
//! storage-plugin binding resolves lazily on first dispatch
//! ([`crate::domain::service::Service::resolve_plugin`]).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{GtsTypeSchema, TypesRegistryClient};
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;
use crate::domain::ports::declarations::DeclarationSource;

/// Reads declarations from `types-registry` through `ClientHub`.
pub struct TypesRegistryDeclarationSource {
    hub: Arc<ClientHub>,
}

impl TypesRegistryDeclarationSource {
    /// Creates a source over `hub`.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl DeclarationSource for TypesRegistryDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        // Resolved per call, not captured at construction: `types-registry`
        // may register after this gear's `init`, and the plugin-host binding
        // in `Service::resolve_plugin` reads the client the same way for the
        // same reason. A hub miss here is an availability fact about the
        // client, not a statement about the type — see `map_registry_error`.
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        registry
            .get_type_schema(id.as_str())
            .await
            .map_err(|err| map_registry_error(id, err))
    }
}

/// Classifies a registry failure into a definite not-found or an
/// availability problem.
///
/// The resolver treats the two differently: it fails closed on the first and
/// may serve a stale declaration for the second, so a misclassification here
/// either hides a withdrawn type or turns an outage into an ingestion stop.
///
/// `CanonicalError` (see `libs/toolkit-canonical-errors`) offers no
/// `is_not_found()` predicate — it exposes `status_code()` and `gts_type()`
/// accessors, but the not-found *category* is the `NotFound` enum variant
/// itself. Matching on it is the established pattern for every other
/// `types-registry` consumer in this workspace (e.g.
/// `license-resolver/src/infra/types_registry.rs`,
/// `account-management/src/infra/types_registry/checker.rs`): `NotFound` is
/// `#[non_exhaustive]`, so the match still needs a catch-all arm, but no
/// field-level pattern is needed beyond `{ .. }`.
fn map_registry_error(id: &MeterTypeId, err: CanonicalError) -> DomainError {
    match err {
        CanonicalError::NotFound { .. } => DomainError::declaration_not_found(id),
        other => DomainError::TypesRegistryUnavailable(other.to_string()),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "types_registry_source_tests.rs"]
mod types_registry_source_tests;
