//! Account Management audit-event shapes.
//!
//! Carries the [`AuditEvent`] / [`AuditActor`] / [`AuditEventKind`]
//! types that AM emits via `emit_audit`. Audit shapes are **internal**
//! to the AM module: only AM itself constructs and emits them. When
//! the platform audit-bus contract lands and external consumers (e.g.
//! an audit-bus plugin) need to match on AM events, the relevant
//! subset can be promoted into the SDK at that point — until then,
//! keeping these types impl-side avoids exporting an unused contract.

use modkit_macros::domain_model;
use modkit_security::SecurityContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Actor attribution on an audit record.
///
/// Either a tenant-scoped caller (derived from `SecurityContext` by
/// the impl-side helper) or the reserved `system` actor used by
/// AM-owned background transitions.
///
/// Wire format is internally-tagged JSON with `camelCase` discriminant
/// and field names — e.g. `{"type":"system"}` /
/// `{"type":"tenantScoped","subjectId":"…","subjectTenantId":"…"}`.
#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum AuditActor {
    /// Actor derived from a validated security context. Carries the
    /// subject and its home tenant.
    #[serde(rename_all = "camelCase")]
    TenantScoped {
        subject_id: Uuid,
        subject_tenant_id: Uuid,
    },
    /// AM-owned background transition. Only events enumerated in
    /// [`AuditEventKind::is_actor_system_eligible`] may use this.
    System,
}

/// Kinds of AM audit events emitted by this module.
///
/// The variants listed in the FEATURE §3 algorithm `audit-emission` step 2
/// as "AM-owned background transitions" are the **only** ones permitted to
/// use [`AuditActor::System`]; all other kinds **MUST** carry a
/// [`AuditActor::TenantScoped`] actor or be dropped by the gate.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum AuditEventKind {
    // ---- actor=system eligible (AM-owned background transitions) ----
    /// Root-tenant bootstrap completed successfully.
    BootstrapCompleted,
    /// Root-tenant bootstrap detected a pre-existing active root and
    /// returned without re-running the saga.
    BootstrapSkipped,
    /// Root-tenant bootstrap found a `provisioning` row and deferred
    /// cleanup to the provisioning reaper.
    BootstrapDeferredToReaper,
    /// Bootstrap exceeded `idp_retry_timeout` while waiting for
    /// `IdpProviderPluginClient::check_availability` and aborted with
    /// `idp_unavailable`.
    BootstrapIdpTimeout,
    /// Bootstrap observed an illegal pre-existing root state
    /// (e.g. suspended or deleted root) and refused to proceed.
    BootstrapInvariantViolation,
    /// Bootstrap finalization step failed after a successful
    /// `provision_tenant`; the row was left in `provisioning` for the
    /// reaper to compensate.
    BootstrapFinalizationFailed,
    /// Conversion request expired without resolution.
    ConversionExpired,
    /// Provisioning reaper compensated an orphaned provisioning.
    ProvisioningReaperCompensated,
    /// Hard-delete cleanup job finished sweeping a tenant's residue.
    HardDeleteCleanupCompleted,
    /// Tenant-deprovision cleanup job finished.
    TenantDeprovisionCompleted,

    // ---- tenant-scoped state-changing transitions ----
    /// Tenant create / status change / mode conversion / metadata write.
    TenantStateChanged,
    /// Conversion request status change driven by a tenant-scoped actor.
    ConversionStateChanged,
    /// Metadata entry written or deleted.
    MetadataWritten,
    /// Hard-delete initiated by a tenant-scoped actor.
    HardDeleteRequested,

    // ---- failure-trail events (per algo-audit-emission step 5) ----
    /// A `cross_tenant_denied` surfacing from the error surface flow.
    CrossTenantDenialRecorded,
    /// An `idp_unavailable` surfacing from the error surface flow.
    IdpUnavailableRecorded,
}

impl AuditEventKind {
    /// Whether this event kind may be emitted with [`AuditActor::System`].
    ///
    /// Matches the authoritative allow-list in `algo-audit-emission`
    /// step 2 — anything outside this set that reaches the gate without a
    /// `SecurityContext` is dropped.
    #[must_use]
    pub const fn is_actor_system_eligible(self) -> bool {
        matches!(
            self,
            Self::BootstrapCompleted
                | Self::BootstrapSkipped
                | Self::BootstrapDeferredToReaper
                | Self::BootstrapIdpTimeout
                | Self::BootstrapInvariantViolation
                | Self::BootstrapFinalizationFailed
                | Self::ConversionExpired
                | Self::ProvisioningReaperCompensated
                | Self::HardDeleteCleanupCompleted
                | Self::TenantDeprovisionCompleted
        )
    }

    /// Stable kind tag used on the `tracing` target and in the emitted
    /// payload. Matches the `Serialize`/`Deserialize` representation of
    /// `Self` byte-for-byte; renaming requires a contract-version review.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BootstrapCompleted => "bootstrapCompleted",
            Self::BootstrapSkipped => "bootstrapSkipped",
            Self::BootstrapDeferredToReaper => "bootstrapDeferredToReaper",
            Self::BootstrapIdpTimeout => "bootstrapIdpTimeout",
            Self::BootstrapInvariantViolation => "bootstrapInvariantViolation",
            Self::BootstrapFinalizationFailed => "bootstrapFinalizationFailed",
            Self::ConversionExpired => "conversionExpired",
            Self::ProvisioningReaperCompensated => "provisioningReaperCompensated",
            Self::HardDeleteCleanupCompleted => "hardDeleteCleanupCompleted",
            Self::TenantDeprovisionCompleted => "tenantDeprovisionCompleted",
            Self::TenantStateChanged => "tenantStateChanged",
            Self::ConversionStateChanged => "conversionStateChanged",
            Self::MetadataWritten => "metadataWritten",
            Self::HardDeleteRequested => "hardDeleteRequested",
            Self::CrossTenantDenialRecorded => "crossTenantDenialRecorded",
            Self::IdpUnavailableRecorded => "idpUnavailableRecorded",
        }
    }
}

/// A fully-prepared audit record waiting for the gate in `emit_audit`.
///
/// Wire format is `camelCase` JSON, consistent with peer SDK conventions
/// (`resource-group-sdk`, `tenant-resolver-sdk`).
// @cpt-begin:cpt-cf-account-management-dod-errors-observability-audit-contract:p1:inst-dod-audit-contract-event-shape
#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AuditEvent {
    pub kind: AuditEventKind,
    pub actor: AuditActor,
    /// The tenant the event describes (not necessarily the actor's home
    /// tenant — e.g. a platform-admin operating on a child tenant).
    ///
    /// `None` is the legitimate value for events that fire **before** any
    /// tenant row exists — currently `BootstrapIdpTimeout` (the `IdP`
    /// availability wait can time out in saga step 0, ahead of the
    /// step-1 `provisioning` row insert that establishes the root
    /// tenant id). Forensics consumers correlate those rows by
    /// `kind` + `actor=system` + the bootstrap-attempt fields in
    /// `payload`. Constructors that take a concrete `Uuid` wrap it as
    /// `Some(...)` automatically; the no-tenant path goes through
    /// [`AuditEvent::system_no_tenant`].
    pub tenant_id: Option<Uuid>,
    /// Free-form structured payload (change diff, diagnostic, request id).
    pub payload: Value,
}
// @cpt-end:cpt-cf-account-management-dod-errors-observability-audit-contract:p1:inst-dod-audit-contract-event-shape

impl AuditEvent {
    /// Build a tenant-scoped event from a validated [`SecurityContext`].
    /// This is the happy-path constructor every AM feature will call.
    ///
    /// # Errors
    ///
    /// Returns [`AnonymousActorNotEligible`] if `ctx` is anonymous —
    /// either built via [`SecurityContext::anonymous`] or with one of
    /// `subject_id` / `subject_tenant_id` left at `Uuid::nil()`. An
    /// `AuditActor::TenantScoped` row with nil-UUID actors is a silent
    /// audit-trail corruption hazard (forensics consumers cannot
    /// distinguish a genuine anonymous request from a buggy caller from
    /// a mislabeled system event), so AM fails loud at the boundary
    /// rather than letting the row land. Background AM transitions
    /// that legitimately have no caller subject **MUST** use
    /// [`AuditEvent::system`] instead.
    pub fn from_context(
        kind: AuditEventKind,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        payload: Value,
    ) -> Result<Self, AnonymousActorNotEligible> {
        let subject_id = ctx.subject_id();
        let subject_tenant_id = ctx.subject_tenant_id();
        // Refuse nil-UUID tenant ids for the same reason we refuse
        // nil-UUID actors: a `Some(Uuid::nil())` row is silent
        // forensics-trail corruption — consumers cannot distinguish a
        // misconfigured caller from a legitimate `None` event (which
        // goes through `system_no_tenant`). Fail loud at the boundary.
        if subject_id.is_nil() || subject_tenant_id.is_nil() || tenant_id.is_nil() {
            return Err(AnonymousActorNotEligible);
        }
        Ok(Self {
            kind,
            actor: AuditActor::TenantScoped {
                subject_id,
                subject_tenant_id,
            },
            tenant_id: Some(tenant_id),
            payload,
        })
    }

    /// Build an `actor=system` event for an AM-owned background transition.
    ///
    /// # Errors
    ///
    /// Returns [`SystemActorNotEligible`] if `kind` is not on the
    /// allow-list — callers **MUST NOT** fabricate `actor=system`
    /// events for unauthorized kinds, and the failure must be loud
    /// rather than silently dropped.
    pub fn system(
        kind: AuditEventKind,
        tenant_id: Uuid,
        payload: Value,
    ) -> Result<Self, SystemActorNotEligible> {
        if !kind.is_actor_system_eligible() || tenant_id.is_nil() {
            // Nil tenant_id is rejected for the same reason as in
            // `from_context`: forensics consumers cannot distinguish
            // it from the legitimate `None` path
            // (`system_no_tenant`). Fail loud rather than land a
            // corrupt row.
            return Err(SystemActorNotEligible { kind });
        }
        Ok(Self {
            kind,
            actor: AuditActor::System,
            tenant_id: Some(tenant_id),
            payload,
        })
    }

    /// Build an `actor=system` event that fires before any tenant row
    /// exists (currently only [`AuditEventKind::BootstrapIdpTimeout`]).
    /// Sets `tenant_id` to `None` rather than forcing the caller to
    /// invent a sentinel/nil UUID that downstream forensics could not
    /// distinguish from a buggy emitter.
    ///
    /// # Errors
    ///
    /// Returns [`SystemActorNotEligible`] for the same reason as
    /// [`AuditEvent::system`] — kinds outside the allow-list are
    /// rejected loud. Additionally rejects kinds that **always** have a
    /// tenant by construction (anything other than the
    /// pre-step-1 bootstrap timeout): forcing them through this
    /// constructor would silently drop the tenant id from forensics.
    pub fn system_no_tenant(
        kind: AuditEventKind,
        payload: Value,
    ) -> Result<Self, SystemActorNotEligible> {
        if !matches!(kind, AuditEventKind::BootstrapIdpTimeout) {
            return Err(SystemActorNotEligible { kind });
        }
        Ok(Self {
            kind,
            actor: AuditActor::System,
            tenant_id: None,
            payload,
        })
    }
}

/// Returned by [`AuditEvent::system`] when the boundary check fails for
/// either of two reasons: (a) the requested `kind` is not on the
/// `actor=system` allow-list defined by
/// [`AuditEventKind::is_actor_system_eligible`], or (b) the supplied
/// `tenant_id` is `Uuid::nil()` (which would land a `Some(Uuid::nil())`
/// row indistinguishable from the legitimate `None` path that goes
/// through [`AuditEvent::system_no_tenant`]). The `kind` field is the
/// requested kind in either case; the Display impl spells out which of
/// the two invariants tripped to keep operator logs actionable.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemActorNotEligible {
    pub kind: AuditEventKind,
}

impl std::fmt::Display for SystemActorNotEligible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.kind.is_actor_system_eligible() {
            write!(
                f,
                "audit event kind '{}' is actor=system-eligible but the \
                 supplied tenant_id was Uuid::nil(); use AuditEvent::system_no_tenant \
                 for events that legitimately have no tenant id",
                self.kind.as_str()
            )
        } else {
            write!(
                f,
                "audit event kind '{}' is not eligible for actor=system; \
                 only AM-owned background transitions may use it",
                self.kind.as_str()
            )
        }
    }
}

impl std::error::Error for SystemActorNotEligible {}

/// Returned by [`AuditEvent::from_context`] when the boundary check
/// fails for any of three reasons: (a) `ctx.subject_id` is
/// [`Uuid::nil`] (anonymous caller), (b) `ctx.subject_tenant_id` is
/// [`Uuid::nil`] (caller's home tenant unknown), or (c) the supplied
/// `tenant_id` is [`Uuid::nil`] (event-target tenant unknown). All
/// three are silent forensics-trail hazards — consumers cannot
/// distinguish a buggy emitter from a legitimate anonymous case — so
/// AM refuses to construct any row that would land a nil-UUID
/// identifier on either side of the actor / target axis.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnonymousActorNotEligible;

impl std::fmt::Display for AnonymousActorNotEligible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "audit boundary refused: caller subject_id, subject_tenant_id, or event tenant_id is Uuid::nil(); \
             nil-UUID identifiers on TenantScoped audit rows are silent forensics corruption -- \
             use AuditEvent::system / system_no_tenant for AM-owned background transitions instead",
        )
    }
}

impl std::error::Error for AnonymousActorNotEligible {}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "audit_tests.rs"]
mod audit_tests;
