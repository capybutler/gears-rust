//! `DeclarationSource` adapter over the `types-registry` SDK client.
//!
//! Resolving the client per call rather than holding it keeps the gear's
//! startup free of an eager `types-registry` dependency, matching how the
//! storage-plugin binding resolves lazily on first dispatch
//! ([`crate::domain::service::Service::resolve_plugin`]).
//!
//! [`build_default_resolvers`] is the bootstrap-layer entry point: it builds
//! this adapter and both resolvers around it, mirroring how
//! [`crate::infra::metrics::build_default_adapter`] builds the metrics
//! adapter. `module.rs` calls it and injects the finished, domain-typed
//! `Arc<TypeResolver>` into `Service::new_with_metrics` — the domain layer
//! never needs to name this adapter type, only the `DeclarationSource` port
//! it implements.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{GtsTypeSchema, RegisterResult, TypesRegistryClient};
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::meter_reverse::MeterReverseResolver;
use crate::domain::ports::declaration_mirror::DeclarationMirror;
use crate::domain::ports::declarations::{DeclarationRegistrar, DeclarationSource};
use crate::domain::ports::metrics::UsageCollectorMetrics;
use crate::domain::type_resolver::{TypeResolver, TypeResolverConfig};

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

    async fn fetch_by_uuid(&self, type_uuid: Uuid) -> Result<GtsTypeSchema, DomainError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        registry
            .get_type_schema_by_uuid(type_uuid)
            .await
            .map_err(|err| map_registry_error_by_uuid(type_uuid, err))
    }
}

#[async_trait]
impl DeclarationRegistrar for TypesRegistryDeclarationSource {
    // @cpt-algo:cpt-cf-usage-collector-algo-mirror-and-restore:p1
    //
    // This method realizes restore-mode's registration-attempt step alone
    // (`inst-restore-register` — "TRY register the stored document back to
    // `types-registry`, under the gear's own identity and never the calling
    // caller's"), including treating a per-item `RegisterResult::Err` as part
    // of that TRY failing rather than as success — ruling J13. Every other
    // restore-mode step lands elsewhere, in a later task: the mode gate
    // (`inst-restore-mode`), the definite-not-found requirement gating
    // whether this runs at all (`inst-restore-definite`), the mirror-row read
    // (`inst-restore-read`), the no-lifecycle-test rule
    // (`inst-restore-no-lifecycle`), the fail-closed rejection on this
    // method's `Err` (`inst-restore-catch` / `inst-restore-fail`), and
    // returning the restored declaration (`inst-restore-return`) all land in
    // `TypeResolver::rehydrate`/`with_rehydration`, not here.
    async fn register(&self, document: &Value) -> Result<(), DomainError> {
        // Resolved per call, for the same reason `fetch` does it.
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        // `e.to_string()` (`Display`), never `e.diagnostic()`. This replaces
        // ruling J24a's `diagnostic()`-with-a-`Display`-fallback, which was
        // safe when it was written and is not safe now:
        //
        // `diagnostic()` (`toolkit-canonical-errors/src/error.rs`) returns
        // `Some(&ctx.description)` for `Internal` and `Unknown`, and that
        // field is `#[serde(skip)]` (`context.rs`, `UnknownV1` and
        // `InternalV1`) — the platform stating, with compiler enforcement,
        // that the text must never be serialized onto a wire. `Display`
        // renders `"{category}: {detail}"`, and `Internal`'s `detail` is the
        // fixed constant `__internal` sets ("An internal error occurred.
        // Please retry later."), so `Display` discloses nothing.
        //
        // Copying `diagnostic()` into this `String` defeated the attribute:
        // the string flows `TypeResolver::restore_document`'s
        // `name_registry_failure` -> four bare `?` in `domain/service.rs` ->
        // `From<DomainError> for UsageCollectorError`, whose
        // `TypesRegistryUnavailable` arm now carries the detail through
        // ->
        // `infra::sdk_error_mapping`'s `ServiceUnavailable` arm ->
        // `CanonicalError::service_unavailable().with_detail(detail)` ->
        // wire `Problem.detail` on the 503. Entry 46 sanctioned naming the
        // meter and the cause on that body; it did not sanction moving a
        // `#[serde(skip)]` description onto it.
        //
        // Nothing is lost for operators: the real cause is already logged
        // server-side by `restore_document`'s `warn!`, which is where the
        // platform's own doc on `diagnostic()` says that description
        // belongs. This also keeps this method's policy identical to
        // `fetch`'s `map_registry_error` below, which has always used
        // `Display`.
        //
        // Pinned by `an_internal_registry_failure_s_diagnostic_never_reaches_
        // the_wire_problem_detail` in this module's tests, which plants a
        // secret in an `Internal` error's description and walks the whole
        // chain above to `Problem.detail`.
        let results = registry
            .register_type_schemas(vec![document.clone()])
            .await
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        // The outer `Ok` is not success. `register_type_schemas` is
        // documented "Returns `Err` only for catastrophic failures" — the
        // per-item outcomes are `register`'s sibling doc's "Per-item failures
        // are reported via `RegisterResult::Err`" — so a refused registration
        // comes back as `Ok(vec![RegisterResult::Err { .. }])` and a bare `?`
        // above would read it as having worked. Spec ruling J13.
        //
        // One input, one output slot: this gear always calls
        // `register_type_schemas` with exactly one document (above), and the
        // real client sizes its result to the input length and writes every
        // index (`types-registry`'s `local_client.rs`), so `results` is a
        // singleton by construction today — never empty, never more than one.
        // The loop is written general on purpose rather than indexing
        // `results[0]`: nothing here asserts the singleton invariant, and a
        // general loop stays correct if a later change ever batches more than
        // one document through this method.
        for result in &results {
            if let RegisterResult::Err { gts_id, error } = result {
                // Back to `{error}` (`Display`), not `{error:?}` — ruling
                // J24b. `Display` carries the refused item's own `gts_id`
                // (formatted in below) and the category name `Display`
                // prefixes onto every `CanonicalError` message.
                //
                // **Ruling J24c's stated reason no longer holds, and the
                // real position is narrower.** J24c argued the string below
                // "is discarded at `domain/error.rs`'s
                // `DomainError`-to-`UsageCollectorError` lift and has no
                // consumer". That stopped being true when ruling K25
                // changed that arm, in a
                // different file, to carry the detail through to the 503's
                // `Problem.detail`. This string now **does** have a
                // consumer, and it is a wire body. It stays `Display` for
                // that reason rather than in spite of it: hand-extracting a
                // field violation's `description` out of `ctx` would put
                // caller-supplied validation text on that body, and
                // `diagnostic()` would be worse still — it returns `None`
                // here (this registry's per-item refusal is
                // `InvalidArgument`-classified) but returns the
                // `#[serde(skip)]` description for `Internal`/`Unknown`,
                // which is the disclosure the outer `map_err` above was
                // changed to avoid.
                return Err(DomainError::TypesRegistryUnavailable(format!(
                    "restoring {} was refused: {error}",
                    gts_id.as_deref().unwrap_or("<no id in the document>"),
                )));
            }
        }

        Ok(())
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

/// [`map_registry_error`]'s reverse-direction sibling.
///
/// A separate function rather than a widened one because the diagnostic
/// differs: there is no `MeterTypeId` to name, so a definite not-found is
/// reported against the reference itself. `DomainError::DeclarationNotFound`
/// carries `gts_type_id` as a `String`, which is what makes that possible
/// without a new variant.
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
///
/// Both read `types-registry` through the same `TypesRegistryDeclarationSource`
/// and write through the same mirror, so a declaration resolved one way
/// populates the cache the other way reads. This is the gear's only
/// production construction point for either.
///
/// The forward half is the Type Resolver: the adapter wrapped in the cache
/// policy `ttl_secs` / `capacity` describe, with `mirror` wired as the
/// DESIGN §3.7 declaration mirror and restore path. It is built through
/// `with_rehydration`, never `new`: `new` defaults to the no-op mirror and
/// would leave this deployment with no restore path at all.
///
/// Called from `module.rs` with the configured `[usage_collector]` cache
/// knobs, the same way [`crate::infra::metrics::build_default_adapter`]
/// builds the metrics adapter from `cfg.metrics.effective_prefix()`.
/// `module.rs` passes that very adapter in as `metrics`, so
/// `uc_type_resolution_total` shares one instrument set with the rest of the
/// gear. Both resolvers are injected into `Service::new_with_metrics` as
/// finished objects; nothing about how they were built leaks into the domain
/// layer.
#[must_use]
pub fn build_default_resolvers(
    hub: Arc<ClientHub>,
    mirror: Arc<dyn DeclarationMirror>,
    ttl_secs: u64,
    capacity: usize,
    metrics: Arc<dyn UsageCollectorMetrics>,
) -> (Arc<TypeResolver>, Arc<MeterReverseResolver>) {
    // ONE adapter instance, handed in three times: it implements both the
    // permanent `DeclarationSource` and the temporary
    // `DeclarationRegistrar`, so the read, the write-back and the reverse
    // read reach `types-registry` through the same object and the same
    // per-call hub lookup.
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
