//! `DeclarationSource` adapter over the `types-registry` SDK client.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{GtsTypeSchema, RegisterResult, TypesRegistryClient};
use usage_collector_sdk::v2::MeterTypeId;
use uuid::Uuid;

use crate::v2::domain::error::DomainError;
use crate::v2::domain::meter_reverse::MeterReverseResolver;
use crate::v2::domain::ports::declaration_mirror::DeclarationMirror;
use crate::v2::domain::ports::declarations::{DeclarationRegistrar, DeclarationSource};
use crate::v2::domain::ports::metrics::UsageCollectorMetrics;
use crate::v2::domain::type_resolver::{TypeResolver, TypeResolverConfig};

/// Bounds a single `types-registry` client call.
const TYPES_REGISTRY_CALL_TIMEOUT: Duration = Duration::from_secs(2);

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
        // Resolved per call so startup does not require `types-registry`.
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        tokio::time::timeout(
            TYPES_REGISTRY_CALL_TIMEOUT,
            registry.get_type_schema(id.as_str()),
        )
        .await
        .map_err(|_| {
            DomainError::TypesRegistryUnavailable(format!(
                "types-registry get_type_schema timed out after {}ms",
                TYPES_REGISTRY_CALL_TIMEOUT.as_millis()
            ))
        })?
        .map_err(|err| map_registry_error(id, err))
    }

    async fn fetch_by_uuid(&self, type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        tokio::time::timeout(
            TYPES_REGISTRY_CALL_TIMEOUT,
            registry.get_type_schema_by_uuid(type_uuid),
        )
        .await
        .map_err(|_| {
            DomainError::TypesRegistryUnavailable(format!(
                "types-registry get_type_schema_by_uuid timed out after {}ms",
                TYPES_REGISTRY_CALL_TIMEOUT.as_millis()
            ))
        })?
        .map_err(|err| map_registry_error_by_uuid(type_uuid, err))
    }
}

#[async_trait]
impl DeclarationRegistrar for TypesRegistryDeclarationSource {
    // @cpt-algo:cpt-cf-usage-collector-algo-mirror-and-restore:p1
    async fn register(&self, document: &Value) -> Result<(), DomainError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        // Use `Display`, not `diagnostic()`: diagnostics can contain
        // `#[serde(skip)]` internal descriptions that must not reach HTTP detail.
        let results = tokio::time::timeout(
            TYPES_REGISTRY_CALL_TIMEOUT,
            registry.register_type_schemas(vec![document.clone()]),
        )
        .await
        .map_err(|_| {
            DomainError::TypesRegistryUnavailable(format!(
                "types-registry register_type_schemas timed out after {}ms",
                TYPES_REGISTRY_CALL_TIMEOUT.as_millis()
            ))
        })?
        .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        // Per-item failures are reported inside the outer `Ok`.
        for result in &results {
            if let RegisterResult::Err { gts_id, error } = result {
                // Keep HTTP detail on the safe `Display` path.
                return Err(DomainError::TypesRegistryUnavailable(format!(
                    "restoring {} was refused: {error}",
                    gts_id.as_deref().unwrap_or("<no id in the document>"),
                )));
            }
        }

        Ok(())
    }
}

/// Classifies a registry failure into a definite not-found or an availability problem.
fn map_registry_error(id: &MeterTypeId, err: CanonicalError) -> DomainError {
    match err {
        CanonicalError::NotFound { .. } => DomainError::declaration_not_found(id),
        other => DomainError::TypesRegistryUnavailable(other.to_string()),
    }
}

/// [`map_registry_error`]'s reverse-direction sibling.
fn map_registry_error_by_uuid(type_uuid: Uuid, err: CanonicalError) -> DomainError {
    match err {
        CanonicalError::NotFound { .. } => DomainError::DeclarationNotFound {
            gts_type_id: type_uuid.to_string(),
            reason: "is not declared under this registry reference".to_owned(),
        },
        other => DomainError::TypesRegistryUnavailable(other.to_string()),
    }
}

/// Build the gear's forward and reverse resolvers over one adapter.
#[must_use]
pub fn build_default_resolvers(
    hub: Arc<ClientHub>,
    mirror: Arc<dyn DeclarationMirror>,
    ttl_secs: u64,
    capacity: usize,
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> (Arc<TypeResolver>, Arc<MeterReverseResolver>) {
    // One adapter backs forward reads, reverse reads, and restore writes.
    let source = Arc::new(TypesRegistryDeclarationSource::new(hub));
    let forward = Arc::new(TypeResolver::with_rehydration(
        Arc::clone(&source) as Arc<dyn DeclarationSource>,
        Arc::clone(&mirror),
        Arc::clone(&source) as Arc<dyn DeclarationRegistrar>,
        TypeResolverConfig {
            ttl: Duration::from_secs(ttl_secs),
            capacity,
        },
        Arc::clone(&metrics),
    ));
    let reverse = Arc::new(MeterReverseResolver::new(
        source as Arc<dyn DeclarationSource>,
        mirror,
        metrics,
    ));
    (forward, reverse)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "types_registry_source_tests.rs"]
mod types_registry_source_tests;
