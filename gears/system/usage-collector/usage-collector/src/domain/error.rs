//! Domain errors for the usage-collector module.
//!
//! Bridges the public SDK envelope ([`UsageCollectorError`]), the plugin-side
//! vocabulary ([`UsageCollectorPluginError`]), and registry / `ClientHub` /
//! plugin-selection failures into the internal [`DomainError`]. The RFC-9457
//! `Problem` lift lives on the REST surface — this module only normalizes
//! failures.
//!
//! There is no catalog error vocabulary any more: every type declaration is
//! owned by `types-registry` and resolved through the Type Resolver, whose
//! unresolvable outcome is [`DomainError::DeclarationNotFound`]. Validation
//! failures (typed SDK `InvalidArgument`s — an invalid `resource_ref`, a
//! metadata key outside the declared shape, an invalidation that departs
//! from its target) flow back to the caller verbatim and are not
//! re-classified through `DomainError`.

use toolkit_macros::domain_model;
use usage_collector_sdk::{
    MeterTypeId, USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginError,
    ValidationReason,
};
use uuid::Uuid;

/// Internal domain errors for the usage-collector host.
#[domain_model]
#[derive(thiserror::Error, Debug, Clone)]
pub enum DomainError {
    #[error("types registry is not available: {0}")]
    TypesRegistryUnavailable(String),

    #[error("no storage plugin instances found for vendor '{vendor}'")]
    PluginNotFound { vendor: String },

    #[error("invalid plugin instance content for '{gts_id}': {reason}")]
    InvalidPluginInstance { gts_id: String, reason: String },

    /// Structural readiness failure on the storage plugin: the selector
    /// resolved an instance but the scoped client is not registered, or
    /// the SDK envelope crossed back into the host without an instance id
    /// in scope. `gts_id` is `Some` only on the cold-path internal call
    /// site that knows which instance was resolved; SDK-envelope lifts
    /// leave it `None` rather than synthesising a placeholder.
    #[error(
        "storage plugin not available{}: {reason}",
        gts_id.as_ref().map(|g| format!(" for '{g}'")).unwrap_or_default()
    )]
    PluginUnavailable {
        gts_id: Option<String>,
        reason: String,
    },

    /// Retryable plugin-reported transient backend failure (downstream
    /// timeout, connection reset, upstream 5xx). Lifts to
    /// `UsageCollectorError::ServiceUnavailable`, preserving the optional
    /// `retry_after_seconds` hint end-to-end.
    #[error("storage plugin transient failure: {detail}")]
    PluginTransient {
        detail: String,
        retry_after_seconds: Option<u64>,
    },

    /// PDP-supplied deny on the requested operation. Surfaces both an explicit
    /// `EnforcerError::Denied` and the fail-closed `EnforcerError::CompileFailed`
    /// branch; both collapse to the same deterministic platform authorization
    /// deny envelope and never derive a permissive fallback.
    #[error("authorization denied{}", reason.as_ref().map(|r| format!(": {r}")).unwrap_or_default())]
    AuthorizationDenied { reason: Option<String> },

    /// The PDP transport failed — `authz-resolver` is unreachable or the
    /// evaluation RPC timed out. The collector fails closed and never serves a
    /// cached or permissive decision, per
    /// `cpt-cf-usage-collector-principle-pdp-centric-authorization`.
    #[error("authorization service unavailable: {0}")]
    AuthorizationUnavailable(String),

    /// Ingestion supplied a `metadata` map carrying a key that is not a
    /// member of the referenced meter's declared `metadata_fields` list
    /// per `cpt-cf-usage-collector-adr-registry-owned-typing` (closed
    /// shape, keyed by `gts_type_id`).
    #[error("unknown metadata key '{key}' for meter {gts_type_id}")]
    UnknownMetadataKey {
        gts_type_id: MeterTypeId,
        key: String,
    },

    /// Idempotency conflict on a usage submission: the supplied
    /// `idempotency_key` is already bound to a different usage submission
    /// (sdk-trait.md §"`DedupOutcome`"). Carries the UUID of the previously
    /// persisted record bound to the key.
    #[error("idempotency conflict: key {idempotency_key} already bound to record {existing_id}")]
    IdempotencyConflict {
        idempotency_key: String,
        existing_id: Uuid,
    },

    /// A lookup referenced a `UsageRecord.id` that does not exist within
    /// the visible scope.
    #[error("usage record not found: {id}")]
    UsageRecordNotFound { id: Uuid },

    /// A submitted invalidation targeted a record that already carries
    /// one. At-most-one-invalidation is the store's obligation, not the
    /// gateway's — only the store can make the check atomic with the entry
    /// it admits (`cpt-cf-usage-collector-adr-append-only-invalidation`) —
    /// so this always arrives lifted from a plugin error, carrying the
    /// invalidation that already withdrew the target.
    #[error("usage record {id} is already invalidated by {invalidated_by}")]
    AlreadyInvalidated { id: Uuid, invalidated_by: Uuid },

    /// The referenced GTS type does not resolve to a usable declaration:
    /// `types-registry` has no row for it, or the row it has does not carry
    /// what a meter needs (a required trait is missing, or it names a fold
    /// this major version does not serve).
    ///
    /// Both collapse to the identical wire failure on purpose. DESIGN §3.2
    /// "Type Resolver" responsibility boundaries say the resolver "does NOT
    /// substitute a default for any declared attribute" and DESIGN §3.3's
    /// ingestion sequence sends every "unresolved" outcome to the same
    /// `NotFound` naming the identifier — so a malformed declaration is
    /// exactly as unresolvable, from every caller's perspective, as an
    /// absent one. Use [`Self::declaration_not_found`] and
    /// [`Self::declaration_incomplete`] to construct this; use
    /// [`Self::is_declaration_not_found`] to test for it.
    #[error("GTS type `{gts_type_id}` {reason}")]
    DeclarationNotFound {
        /// The unresolvable meter reference.
        gts_type_id: String,
        /// Why it does not resolve: `"is not declared"` for a genuine
        /// not-found answer from the registry, or the specific
        /// missing/unserved trait otherwise.
        reason: String,
    },

    /// A submitted entry's `metadata` falls outside the meter's declared
    /// closed surface: an undeclared key, or a value violating a declared
    /// constraint (`CompiledMetadataSchema::validate`). `0` joins every
    /// violation `jsonschema` reports, not just the first, so a caller
    /// correcting a payload sees all of them at once.
    #[error("metadata does not match the declared schema: {0}")]
    InvalidMetadata(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// `types-registry` has no declaration under this identifier.
    ///
    /// Distinct from [`Self::TypesRegistryUnavailable`]: this is a definite
    /// answer, not a transport failure, so a cache built on top of this port
    /// (Task 6) can act on it directly rather than riding it out on a stale
    /// entry. Test for it with [`Self::is_declaration_not_found`].
    #[must_use]
    pub fn declaration_not_found(id: &MeterTypeId) -> Self {
        Self::DeclarationNotFound {
            gts_type_id: id.as_str().to_owned(),
            reason: "is not declared".to_owned(),
        }
    }

    /// A declaration that resolved but does not carry what a meter needs:
    /// `why` names the missing required trait, or the fold this major
    /// version does not serve.
    ///
    /// Fails closed exactly like [`Self::declaration_not_found`] — the gear
    /// never treats an incomplete declaration as more resolvable than an
    /// absent one, so the two constructors build the same variant.
    #[must_use]
    pub fn declaration_incomplete(id: &MeterTypeId, why: &str) -> Self {
        Self::DeclarationNotFound {
            gts_type_id: id.as_str().to_owned(),
            reason: why.to_owned(),
        }
    }

    /// True when this is a definite "does not resolve" answer rather than a
    /// possibly-transient availability failure. The `DeclarationSource` port
    /// contract (and Task 6's cache) depends on telling the two apart: only
    /// this case is safe to act on immediately rather than served from a
    /// stale cached entry.
    #[must_use]
    pub fn is_declaration_not_found(&self) -> bool {
        matches!(self, Self::DeclarationNotFound { .. })
    }

    /// Metadata outside the meter's declared closed surface.
    ///
    /// `detail` is the `"; "`-joined text of every violation
    /// `CompiledMetadataSchema::validate` collected, not just the first —
    /// see its doc comment for why.
    #[must_use]
    pub fn invalid_metadata(detail: impl Into<String>) -> Self {
        Self::InvalidMetadata(detail.into())
    }
}

// NOTE(DE1302): `DomainError::Internal` / `TypesRegistryUnavailable` only carry
// a String, so these From impls intentionally stringify the source error.
//
// A `TypesRegistryClient::list_instances` failure is a registry-availability
// failure on the lazy storage-plugin resolution path, so it maps to
// `TypesRegistryUnavailable` (not `Internal`): the selector cache stays empty
// and the next dispatch retries the resolve, per the Plugin Host binding algo.
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<types_registry_sdk::TypesRegistryError> for DomainError {
    fn from(e: types_registry_sdk::TypesRegistryError) -> Self {
        Self::TypesRegistryUnavailable(e.to_string())
    }
}

#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<toolkit::client_hub::ClientHubError> for DomainError {
    fn from(e: toolkit::client_hub::ClientHubError) -> Self {
        Self::Internal(e.to_string())
    }
}

#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<serde_json::Error> for DomainError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<toolkit::plugins::ChoosePluginError> for DomainError {
    fn from(e: toolkit::plugins::ChoosePluginError) -> Self {
        match e {
            toolkit::plugins::ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                Self::InvalidPluginInstance { gts_id, reason }
            }
            toolkit::plugins::ChoosePluginError::PluginNotFound { vendor, .. } => {
                Self::PluginNotFound { vendor }
            }
        }
    }
}

// Fail-closed: `CompileFailed` collapses to `AuthorizationDenied` (the
// collector calls the enforcer without `require_constraints(true)`, so a
// non-deny `CompileFailed` is unexpected — but we keep the mapping
// fail-closed). `EvaluationFailed` is the only path to
// `AuthorizationUnavailable`.
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-entity-pdp-decision:p1
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-principle-fail-closed:p2
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    fn from(e: authz_resolver_sdk::EnforcerError) -> Self {
        use authz_resolver_sdk::EnforcerError;
        match e {
            // @cpt-begin:cpt-cf-usage-collector-flow-foundation-pdp-authorize:p1:inst-pdp-deny
            // @cpt-begin:cpt-cf-usage-collector-algo-foundation-pdp-authorize:p2:inst-algo-pdp-deny
            // Reason is captured by field extraction (NOT `{r:?}`) so a future
            // `DenyReason` field addition cannot leak struct internals into
            // operator logs. The wire envelope drops the reason; this string
            // only feeds the `tracing` Display path.
            EnforcerError::Denied { deny_reason } => Self::AuthorizationDenied {
                reason: deny_reason.map(|r| match r.details {
                    Some(details) => format!("{}: {details}", r.error_code),
                    None => r.error_code,
                }),
            },
            EnforcerError::CompileFailed(err) => Self::AuthorizationDenied {
                reason: Some(err.to_string()),
            },
            // @cpt-end:cpt-cf-usage-collector-algo-foundation-pdp-authorize:p2:inst-algo-pdp-deny
            // @cpt-end:cpt-cf-usage-collector-flow-foundation-pdp-authorize:p1:inst-pdp-deny
            // @cpt-begin:cpt-cf-usage-collector-flow-foundation-pdp-authorize:p1:inst-pdp-resolver-catch
            // @cpt-begin:cpt-cf-usage-collector-flow-foundation-pdp-authorize:p1:inst-pdp-fail-closed
            // @cpt-begin:cpt-cf-usage-collector-algo-foundation-pdp-authorize:p2:inst-algo-pdp-catch
            // @cpt-begin:cpt-cf-usage-collector-algo-foundation-pdp-authorize:p2:inst-algo-pdp-fail-closed
            EnforcerError::EvaluationFailed(err) => Self::AuthorizationUnavailable(err.to_string()),
            // @cpt-end:cpt-cf-usage-collector-algo-foundation-pdp-authorize:p2:inst-algo-pdp-fail-closed
            // @cpt-end:cpt-cf-usage-collector-algo-foundation-pdp-authorize:p2:inst-algo-pdp-catch
            // @cpt-end:cpt-cf-usage-collector-flow-foundation-pdp-authorize:p1:inst-pdp-fail-closed
            // @cpt-end:cpt-cf-usage-collector-flow-foundation-pdp-authorize:p1:inst-pdp-resolver-catch
        }
    }
}

// `UsageCollectorPluginError` is `#[non_exhaustive]`. The catch-all arm exists
// only for future variant growth; the `is_*_exhaustive_today` debug_assert
// fires in tests if a new variant is added without extending the match.
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<UsageCollectorPluginError> for DomainError {
    fn from(e: UsageCollectorPluginError) -> Self {
        debug_assert!(is_plugin_error_exhaustive_today(&e));
        match e {
            UsageCollectorPluginError::Transient {
                detail,
                retry_after_seconds,
            } => Self::PluginTransient {
                detail,
                retry_after_seconds,
            },
            UsageCollectorPluginError::Internal(detail) => Self::Internal(detail),
            UsageCollectorPluginError::IdempotencyConflict {
                idempotency_key,
                existing_id,
            } => Self::IdempotencyConflict {
                idempotency_key,
                existing_id,
            },
            UsageCollectorPluginError::UsageRecordNotFound { id } => {
                Self::UsageRecordNotFound { id }
            }
            UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by } => {
                Self::AlreadyInvalidated { id, invalidated_by }
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

#[allow(dead_code)]
fn is_plugin_error_exhaustive_today(e: &UsageCollectorPluginError) -> bool {
    matches!(
        e,
        UsageCollectorPluginError::Transient { .. }
            | UsageCollectorPluginError::Internal(_)
            | UsageCollectorPluginError::IdempotencyConflict { .. }
            | UsageCollectorPluginError::UsageRecordNotFound { .. }
            | UsageCollectorPluginError::AlreadyInvalidated { .. }
    )
}

// The public envelope → DomainError direction is intentionally absent:
// the compacted `UsageCollectorError` is a terminal caller-facing shape and
// nothing re-classifies it back into the host-internal vocabulary. The
// host always flows DomainError → UsageCollectorError (below).

impl From<DomainError> for UsageCollectorError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::PluginNotFound { .. } | DomainError::PluginUnavailable { .. } => {
                Self::plugin_unavailable()
            }
            DomainError::PluginTransient {
                detail,
                retry_after_seconds,
            } => Self::service_unavailable(detail, retry_after_seconds),
            DomainError::AuthorizationDenied { reason } => {
                Self::permission_denied(reason.unwrap_or_else(|| "denied".to_owned()))
            }
            DomainError::AuthorizationUnavailable(reason) => {
                Self::service_unavailable(reason, None)
            }
            DomainError::UnknownMetadataKey { gts_type_id, key } => {
                Self::unknown_metadata_key(&gts_type_id, &key)
            }
            DomainError::IdempotencyConflict {
                idempotency_key,
                existing_id,
            } => Self::idempotency_conflict(&idempotency_key, existing_id),
            DomainError::UsageRecordNotFound { id } => Self::usage_record_not_found(id),
            DomainError::AlreadyInvalidated { id, invalidated_by } => {
                Self::already_invalidated(id, invalidated_by)
            }
            // DESIGN §3.3: an unresolvable GTS type is a 404 naming the
            // identifier, whether the registry never declared it or the
            // Type Resolver rejected an incomplete declaration for it — both
            // reach this same arm because both build
            // `DomainError::DeclarationNotFound` (see its doc comment).
            //
            // `resource_type` is `USAGE_RECORD_RESOURCE`, not a usage-type
            // marker: `gts_type_id` is a `usage_record`-derived meter type,
            // and this gear declares no other GTS resource on its wire
            // surface now that types-registry owns the catalog.
            DomainError::DeclarationNotFound {
                gts_type_id,
                reason,
            } => {
                let detail = format!("GTS type `{gts_type_id}` {reason}");
                Self::NotFound {
                    resource_type: USAGE_RECORD_RESOURCE.to_owned(),
                    name: gts_type_id,
                    detail,
                }
            }
            DomainError::InvalidPluginInstance { gts_id, reason } => {
                Self::internal(format!("invalid plugin instance '{gts_id}': {reason}"))
            }
            // Attributed to the record surface (not a specific `gts_id`):
            // `CompiledMetadataSchema::validate` has no usage-type identity
            // in scope, only the entry's own metadata map.
            DomainError::InvalidMetadata(detail) => Self::InvalidArgument {
                resource_type: USAGE_RECORD_RESOURCE.to_owned(),
                resource_name: None,
                field: "metadata".to_owned(),
                reason: ValidationReason::MetadataValidation,
                detail,
            },
            DomainError::TypesRegistryUnavailable(_) => Self::types_registry_unavailable(),
            DomainError::Internal(reason) => Self::internal(reason),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
