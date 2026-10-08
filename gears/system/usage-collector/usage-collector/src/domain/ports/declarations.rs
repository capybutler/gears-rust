//! Port for reading GTS type declarations.
//!
//! The Type Resolver depends on this narrow port rather than the whole
//! `TypesRegistryClient`, so its caching policy can be exercised against a
//! trivial fake and the real registry adapter stays in `infra` — the
//! `domain/ports/` shape the gear already uses for metrics.

use async_trait::async_trait;
use serde_json::Value;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

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

    /// Fetches the type schema for the registry reference `type_uuid`.
    ///
    /// The reverse of [`Self::fetch`]. `types-registry` is authoritative for
    /// this direction: the returned schema carries the exact identifier the
    /// reference was issued for.
    ///
    /// # Errors
    ///
    /// - [`DomainError::DeclarationNotFound`] when the registry gives a
    ///   definite not-found answer for the reference.
    /// - [`DomainError::TypesRegistryUnavailable`] for any other failure.
    async fn fetch_by_uuid(&self, type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError>;
}

/// [`DeclarationSource`] with no working backend.
///
/// A domain-owned placeholder for contexts that need *an* implementation to
/// construct a [`crate::domain::type_resolver::TypeResolver`] but have no real
/// adapter, mirroring [`crate::domain::ports::metrics::NoopMetrics`]. Every
/// call reports the registry as unavailable rather than panicking.
///
/// Used only by [`crate::domain::service::Service::new`]'s convenience
/// default, which cannot reach into `infra` for the real adapter without
/// reintroducing the domain → infra edge this port exists to prevent.
/// Production bootstrap always injects an adapter-backed resolver through
/// [`crate::domain::service::Service::new_with_metrics`].
#[allow(dead_code)] // constructed only by Service::new's convenience default
pub struct UnavailableDeclarationSource;

#[async_trait]
impl DeclarationSource for UnavailableDeclarationSource {
    async fn fetch(&self, _id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        Err(DomainError::TypesRegistryUnavailable(
            "Service::new has no types-registry adapter wired".to_owned(),
        ))
    }

    async fn fetch_by_uuid(&self, _type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError> {
        Err(DomainError::TypesRegistryUnavailable(
            "Service::new has no types-registry adapter wired".to_owned(),
        ))
    }
}

/// Registers a declaration back into `types-registry`.
///
/// **Temporary**, with the rest of the rehydration bridge:
/// `cpt-cf-usage-collector-adr-declaration-rehydration` statement 7 deletes
/// the restore when `types-registry` gains persistent storage on this gear's
/// resolution path. It sits beside the permanent [`DeclarationSource`]
/// rather than inside it, so retiring the bridge deletes this trait and its
/// `impl` instead of leaving a dead method on a port that stays.
///
/// **This trait takes a document and nothing else, deliberately.**
/// `usage-type-resolution.md`'s `inst-restore-register` step requires the
/// write to run *"under the gear's own identity and never the calling
/// caller's"*, and a signature with nowhere to put a principal realizes that
/// at compile time rather than by a convention a reviewer has to check. What
/// bounds the write is its input: the resolver replays a document
/// `types-registry` itself returned earlier, so no caller can introduce a
/// declaration the platform has not already accepted. The `@cpt-algo` marker
/// for the step lives on the realizing code in
/// `infra/types_registry_source.rs`, not on this bodyless signature.
#[async_trait]
pub trait DeclarationRegistrar: Send + Sync + 'static {
    /// Registers `document` as a GTS type-schema.
    ///
    /// # Errors
    ///
    /// [`DomainError::TypesRegistryUnavailable`] when the registry cannot be
    /// reached, the call fails catastrophically, **or any returned
    /// `RegisterResult` is an `Err`** — a per-item refusal arrives inside an
    /// `Ok`, and reading it as success would serve a declaration the registry
    /// does not hold (spec ruling J13). The variant carries all of these
    /// rather than claiming the registry is unreachable: this port's only
    /// caller fails closed on each identically, and `DomainError` has no
    /// invalid-argument variant reachable from this seam.
    async fn register(&self, document: &Value) -> Result<(), DomainError>;
}

/// [`DeclarationRegistrar`] with no working backend — the default
/// [`crate::domain::type_resolver::TypeResolver::new`] supplies.
///
/// [`UnavailableDeclarationSource`]'s counterpart for the write port.
/// `register` reports the registry as unavailable rather than panicking, so a
/// restore attempted against it **fails closed** — the right outcome for a
/// resolver built without the bridge, and the reason this one refuses where
/// [`super::declaration_mirror::NoopDeclarationMirror`] deliberately
/// succeeds: a no-op mirror read already answers "no row", so the restore
/// never reaches this registrar on that path (controller ruling J18).
///
/// Production uses
/// [`crate::domain::type_resolver::TypeResolver::with_rehydration`] with the
/// real adapter; this type is reached only through `TypeResolver::new`.
pub struct UnavailableDeclarationRegistrar;

#[async_trait]
impl DeclarationRegistrar for UnavailableDeclarationRegistrar {
    async fn register(&self, _document: &Value) -> Result<(), DomainError> {
        Err(DomainError::TypesRegistryUnavailable(
            "Service::new has no types-registry adapter wired".to_owned(),
        ))
    }
}
