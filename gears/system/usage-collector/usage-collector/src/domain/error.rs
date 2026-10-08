//! Domain errors for the usage-collector module.
//!
//! Bridges the public SDK envelope ([`UsageCollectorError`]), the plugin-side
//! vocabulary ([`UsageCollectorPluginError`]), and registry / `ClientHub` /
//! plugin-selection failures into the internal [`DomainError`]. The RFC-9457
//! `Problem` lift lives on the REST surface — this module only normalizes
//! failures.
//!
//! There is no catalog error vocabulary: every type declaration is owned by
//! `types-registry` and resolved through the Type Resolver, whose unresolvable
//! outcome is [`DomainError::DeclarationNotFound`]. Validation failures (typed
//! SDK `InvalidArgument`s) flow back to the caller verbatim rather than being
//! re-classified through `DomainError`.

use toolkit_macros::domain_model;
use usage_collector_sdk::{
    AlreadyInvalidatedArgs, Invalidation, MeterTypeId, NotFoundReason, ReasonCode,
    USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginError, ValidationReason,
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
    /// `idempotency_key` is already bound to a different usage submission.
    /// Carries the UUID of the previously persisted record bound to the key.
    #[error("idempotency conflict: key {idempotency_key} already bound to record {existing_id}")]
    IdempotencyConflict {
        idempotency_key: String,
        existing_id: Uuid,
    },

    /// A lookup referenced a `UsageRecord.id` that does not exist within
    /// the visible scope.
    #[error("usage record not found: {id}")]
    UsageRecordNotFound { id: Uuid },

    /// An invalidation target lookup answered `UsageRecordNotConverged`.
    /// Raised only at the two target lookups, which read converged-only.
    ///
    /// The gear-visible half of `cpt-cf-usage-collector-state-dedup-identity`:
    /// the gear keeps no state of its own, but the converged-only target lookup
    /// this variant backs is one of the downstream features that read that state
    /// directly — `record-invalidation` refuses an `Acknowledged`
    /// (not-yet-converged) target rather than treating it as absent.
    ///
    /// **That identifier stays unticked**, not because of this variant but
    /// because its `Released → Unused` transition asserts a period-bound refusal
    /// that runs "before deduplication is consulted". No such refusal exists on
    /// the backfill route (see `covered_period.rs::ingestion_action`), so an
    /// over-aged period is **not** unreachable the way the transition claims —
    /// the same disposition as the sibling
    /// `cpt-cf-usage-collector-dod-retention-horizon-containment`.
    #[error("invalidation target {target} has not converged")]
    TargetNotConverged { target: Uuid },

    /// A storage plugin refused a cursor because retention has already removed
    /// an entry the continuation would have to deliver.
    #[error("cursor names a position retention has already passed")]
    CursorBeyondRetention,

    /// A dispatched invalidation collided with a stored invalidation of the
    /// same target under another reason code. Built by
    /// [`lift_dispatch_error`], never by the context-free `From`.
    #[error("usage record {target} is already invalidated by {invalidated_by}")]
    AlreadyInvalidated {
        target: Uuid,
        invalidated_by: Uuid,
        reason_code: ReasonCode,
    },

    /// The referenced GTS type does not resolve to a usable declaration:
    /// `types-registry` has no row for it, or the row it has does not carry
    /// what a meter needs (a required trait is missing, or it names a fold
    /// this major version does not serve).
    ///
    /// Both collapse to the identical wire failure on purpose: DESIGN §3.2 says
    /// the resolver "does NOT substitute a default for any declared attribute"
    /// and §3.3's ingestion sequence sends every "unresolved" outcome to the same
    /// `NotFound` naming the identifier, so a malformed declaration is exactly as
    /// unresolvable as an absent one. Construct with
    /// [`Self::declaration_not_found`] / [`Self::declaration_incomplete`]; test
    /// with [`Self::is_declaration_not_found`].
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
    /// answer, not a transport failure, so the Type Resolver's cache, built
    /// on top of this port, can act on it directly rather than riding it out
    /// on a stale entry. Test for it with [`Self::is_declaration_not_found`].
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
    /// contract and the Type Resolver's cache depend on telling the two apart:
    /// only this case is safe to act on immediately rather than served from a
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
// `TypesRegistryUnavailable`: the selector cache stays empty and the next
// dispatch retries the resolve, per the Plugin Host binding algo.
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

// Fail-closed: `CompileFailed` collapses to `AuthorizationDenied`. Every
// collector call site sets `require_constraints(true)`, so a non-deny
// `CompileFailed` is unexpected, but the mapping stays fail-closed.
// `EvaluationFailed` is the only path to `AuthorizationUnavailable`.
// @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p2
// @cpt-algo:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    fn from(e: authz_resolver_sdk::EnforcerError) -> Self {
        use authz_resolver_sdk::EnforcerError;
        match e {
            // @cpt-begin:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-failmap-deny
            // @cpt-begin:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-algo-pdp-deny
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
            // @cpt-end:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-algo-pdp-deny
            // @cpt-end:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-failmap-deny
            EnforcerError::CompileFailed(err) => Self::AuthorizationDenied {
                reason: Some(err.to_string()),
            },
            // @cpt-begin:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-failmap-unavailable
            // @cpt-begin:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-pdp-fail-closed
            // @cpt-begin:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-algo-pdp-catch
            // @cpt-begin:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-algo-pdp-fail-closed
            EnforcerError::EvaluationFailed(err) => Self::AuthorizationUnavailable(err.to_string()),
            // @cpt-end:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-algo-pdp-fail-closed
            // @cpt-end:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-algo-pdp-catch
            // @cpt-end:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-pdp-fail-closed
            // @cpt-end:cpt-cf-usage-collector-algo-fail-closed-outcome-mapping:p1:inst-failmap-unavailable
        }
    }
}

// `UsageCollectorPluginError` is `#[non_exhaustive]`. The catch-all arm exists
// only for future variant growth; the `is_*_exhaustive_today` debug_assert
// fires in tests if a new variant is added without extending the match.
// @cpt-dod:cpt-cf-usage-collector-dod-plugin-error-mapping:p1
// @cpt-algo:cpt-cf-usage-collector-algo-plugin-error-classification:p1
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
            // Which conflict this is depends on the entry that was dispatched,
            // which this conversion cannot see. `lift_dispatch_error` is the
            // lift for a dispatch result; reaching here is a host breach.
            UsageCollectorPluginError::IdempotencyConflict { existing, .. } => {
                Self::Internal(format!(
                    "storage plugin conflict on {} was lifted without its dispatched entry",
                    existing.id
                ))
            }
            UsageCollectorPluginError::UsageRecordNotFound { id } => {
                Self::UsageRecordNotFound { id }
            }
            UsageCollectorPluginError::UsageRecordNotConverged { id } => Self::Internal(format!(
                "storage plugin answered UsageRecordNotConverged for {id} outside a converged-only lookup"
            )),
            UsageCollectorPluginError::CursorBeyondRetention => Self::CursorBeyondRetention,
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
            | UsageCollectorPluginError::UsageRecordNotConverged { .. }
            | UsageCollectorPluginError::CursorBeyondRetention
    )
}

// The public envelope → DomainError direction is intentionally absent:
// the compacted `UsageCollectorError` is a terminal caller-facing shape and
// nothing re-classifies it back into the host-internal vocabulary. The
// host always flows DomainError → UsageCollectorError (below).

// @cpt-dod:cpt-cf-usage-collector-dod-fail-closed-outcomes:p1
impl From<DomainError> for UsageCollectorError {
    fn from(e: DomainError) -> Self {
        match e {
            // Config-free fallback: a bare `From` has no
            // `unavailable_retry_after_secs` to apply. Production never reaches
            // it for these variants — `lift_domain_error` below intercepts both
            // — so this is reached only by a direct conversion in a test.
            DomainError::PluginNotFound { .. } | DomainError::PluginUnavailable { .. } => {
                Self::plugin_unavailable(None)
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
            // Same posture as the `PluginNotFound` / `PluginUnavailable` arm:
            // config-free fallback. `resolve_invalidation_targets` and the
            // single-entry withdrawal path both intercept this variant with the
            // configured `target_not_converged_retry_after_secs` first.
            DomainError::TargetNotConverged { target } => Self::target_not_converged(target, None),
            DomainError::AlreadyInvalidated {
                target,
                invalidated_by,
                reason_code,
            } => Self::already_invalidated(AlreadyInvalidatedArgs {
                target,
                invalidated_by,
                reason_code,
            }),
            // DESIGN §3.3: an unresolvable GTS type is a 404 naming the
            // identifier, whether the registry never declared it or the Type
            // Resolver rejected an incomplete declaration — both build
            // `DomainError::DeclarationNotFound`.
            //
            // `resource_type` is `USAGE_RECORD_RESOURCE`, not a usage-type
            // marker: `gts_type_id` is a `usage_record`-derived meter type, and
            // this gear declares no other GTS resource on its wire surface.
            // `reason` is bound as `why`, matching `declaration_incomplete`'s own
            // parameter, so it does not read as the typed `NotFoundReason` set
            // below it: the domain field is prose, the SDK field a
            // discriminator.
            DomainError::DeclarationNotFound {
                gts_type_id,
                reason: why,
            } => {
                let detail = format!("GTS type `{gts_type_id}` {why}");
                Self::NotFound {
                    resource_type: USAGE_RECORD_RESOURCE.to_owned(),
                    name: gts_type_id,
                    reason: NotFoundReason::DeclarationNotFound,
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
            DomainError::CursorBeyondRetention => Self::InvalidArgument {
                resource_type: USAGE_RECORD_RESOURCE.to_owned(),
                resource_name: None,
                field: "cursor".to_owned(),
                reason: ValidationReason::CursorBeyondRetention,
                detail: "retention has removed an entry after the position this cursor names, so \
                         the continuation cannot be served whole; restart the subscription from \
                         its oldest retained position"
                    .to_owned(),
            },
            // Config-free fallback, same posture as `PluginNotFound` /
            // `PluginUnavailable` above. This arm serves production for the Type
            // Resolver's own `TypesRegistryUnavailable` (a `DeclarationSource`
            // adapter failure), a different origin from the Plugin Host's
            // registry lookup inside `get_plugin`, which `lift_domain_error`
            // intercepts instead. DESIGN §3.8's `unavailable_retry_after_secs`
            // names "a dispatch that reached no plugin at all", so the Type
            // Resolver's occurrence of the variant is deliberately left carrying
            // no delay.
            //
            // `detail` is carried through rather than replaced by the SDK's
            // fixed `types_registry_unavailable` string:
            // `TypeResolver::name_registry_failure` wraps the failing identifier
            // into it, which is what makes a fail-closed 503 on this path name
            // the meter. The Plugin Host's own `TypesRegistryUnavailable` (via
            // `lift_domain_error`, not this arm) never carried one.
            DomainError::TypesRegistryUnavailable(detail) => {
                Self::service_unavailable(detail, None)
            }
            DomainError::Internal(reason) => Self::internal(reason),
        }
    }
}

/// Lift a `create_usage_records` outcome, knowing what was dispatched.
///
/// A store `IdempotencyConflict` is reported by the kind of the **dispatched**
/// entry (DESIGN §3.3 error lift table):
/// - on a record, as `IdempotencyConflict` naming the stored entry;
/// - on an invalidation, as `AlreadyInvalidated` naming the target, the stored
///   invalidation and its reason code. Every invalidation of one record repeats
///   that record's tenant, type, key and period and reads
///   `entry_type = invalidation`, so all of them reach one dedup identity and a
///   conflict can only be against another invalidation of that target; a stored
///   entry that is not one is a plugin breach.
///
/// Every other error takes the context-free `From`.
///
/// This function is the collision arm of
/// `cpt-cf-usage-collector-algo-invalidation-collision-outcome` and of
/// `cpt-cf-usage-collector-dod-single-invalidation-per-entry`; the persisted,
/// absorbed and transient arms are `crate::domain::service`'s
/// `settle_dispatched`, which calls it. The `None` arm below is that `DoD`'s last
/// sentence — "A plugin naming a stored entry whose entry type differs from the
/// dispatched one MUST surface as an internal failure rather than as an
/// already-invalidated conflict" — reachable only through a plugin breaching the
/// dedup identity.
// @cpt-algo:cpt-cf-usage-collector-algo-invalidation-collision-outcome:p1
// @cpt-dod:cpt-cf-usage-collector-dod-single-invalidation-per-entry:p1
pub(crate) fn lift_dispatch_error(
    err: UsageCollectorPluginError,
    dispatched_invalidation: Option<&Invalidation>,
) -> DomainError {
    let UsageCollectorPluginError::IdempotencyConflict {
        idempotency_key,
        existing,
    } = err
    else {
        return DomainError::from(err);
    };
    let existing = *existing;
    let Some(dispatched) = dispatched_invalidation else {
        return DomainError::IdempotencyConflict {
            idempotency_key,
            existing_id: existing.id,
        };
    };
    match existing.invalidation {
        Some(stored) => DomainError::AlreadyInvalidated {
            target: dispatched.target,
            invalidated_by: existing.id,
            reason_code: stored.reason,
        },
        None => DomainError::Internal(format!(
            "storage plugin reported entry {} as the stored invalidation of {}, but it carries no invalidation",
            existing.id, dispatched.target
        )),
    }
}

/// `unavailable_retry_after_secs` for [`lift_domain_error`], distinguished by
/// type from [`TargetNotConvergedRetryAfterSecs`] so the two delays can't be
/// transposed at a call site without a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnavailableRetryAfterSecs(pub(crate) u64);

/// `target_not_converged_retry_after_secs` for [`lift_domain_error`],
/// distinguished by type from [`UnavailableRetryAfterSecs`] so the two
/// delays can't be transposed at a call site without a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TargetNotConvergedRetryAfterSecs(pub(crate) u64);

/// Lifts a [`DomainError`] onto [`UsageCollectorError`], resolving the two
/// DESIGN §3.8 retry-delay defaults a bare [`From`] conversion cannot see —
/// a configuration value, not a fact derivable from the error alone:
///
/// - `unavailable_retry_after_secs` backs a `ServiceUnavailable` that
///   reached no hint of its own: a dispatch that reached no plugin at all
///   (`DomainError::PluginNotFound`, `DomainError::PluginUnavailable`, and
///   `DomainError::TypesRegistryUnavailable` **when it originates from the
///   Plugin Host's own registry lookup inside `Service::resolve_plugin`**),
///   or a plugin `Transient` that itself carried no hint
///   (`DomainError::PluginTransient` with `retry_after_seconds: None`). A
///   plugin-supplied hint always wins over this default.
///
///   A second origin reaches this same arm: `get_usage_record`'s reverse
///   resolution of a stored reference back to a `MeterTypeId` falls through to
///   the Type Resolver's `DeclarationSource` on its bottom tier, and an outage
///   there is a `TypesRegistryUnavailable` too. That call site routes it through
///   this function, so it gets the same retry hint — but not `detail`: this arm
///   substitutes the SDK's fixed `types_registry_unavailable` string, which is
///   acceptable because `detail` on that path would name an internal registry
///   reference the caller never supplied, where on the ingestion path it names
///   the meter the caller did.
/// - `target_not_converged_retry_after_secs` backs
///   `DomainError::TargetNotConverged`'s `Conflict(TargetNotConverged)`.
///
/// Every other variant takes the context-free [`From`] unchanged. This function
/// exists to be called where the call site knows which origin a
/// `TypesRegistryUnavailable` or `PluginTransient` in hand actually has
/// (`Service::resolve_plugin_for`, every plugin-SPI dispatch catch site, and
/// `get_usage_record`'s reverse-resolver catch site), not to replace `From` as
/// the fallback: a Type-Resolver-triggered `TypesRegistryUnavailable` still
/// answers `None` everywhere else.
pub(crate) fn lift_domain_error(
    e: DomainError,
    unavailable_retry_after_secs: UnavailableRetryAfterSecs,
    target_not_converged_retry_after_secs: TargetNotConvergedRetryAfterSecs,
) -> UsageCollectorError {
    match e {
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-error-classification:p1:inst-err-host-unavailable
        DomainError::TypesRegistryUnavailable(_) => {
            UsageCollectorError::types_registry_unavailable(Some(unavailable_retry_after_secs.0))
        }
        DomainError::PluginNotFound { .. } | DomainError::PluginUnavailable { .. } => {
            UsageCollectorError::plugin_unavailable(Some(unavailable_retry_after_secs.0))
        }
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-error-classification:p1:inst-err-host-unavailable
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-error-classification:p1:inst-err-transient
        DomainError::PluginTransient {
            detail,
            retry_after_seconds,
        } => UsageCollectorError::service_unavailable(
            detail,
            Some(retry_after_seconds.unwrap_or(unavailable_retry_after_secs.0)),
        ),
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-error-classification:p1:inst-err-transient
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-error-classification:p1:inst-err-not-converged
        DomainError::TargetNotConverged { target } => UsageCollectorError::target_not_converged(
            target,
            Some(target_not_converged_retry_after_secs.0),
        ),
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-error-classification:p1:inst-err-not-converged
        other => UsageCollectorError::from(other),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
