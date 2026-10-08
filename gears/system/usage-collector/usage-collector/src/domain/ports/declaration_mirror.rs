//! Port for the declaration mirror of DESIGN §3.7.
//!
//! **Temporary.** `cpt-cf-usage-collector-adr-declaration-rehydration`
//! statement 7: the mirror and the restore *"are deleted when persistent
//! storage reaches this gear's resolution path"*, and statement 6 — the
//! plugin reading declared retention from `types-registry` itself —
//! *"outlives them"*. Retiring the bridge deletes this file, its sibling
//! `DeclarationRegistrar`, `crate::infra::declaration_mirror`, and
//! `TypeResolver::rehydrate`.

use async_trait::async_trait;
use serde_json::Value;
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

/// A mirror operation that failed.
///
/// **Deliberately NOT a [`crate::domain::error::DomainError`].** A failed
/// mirror **write** never reaches a caller — ADR statement 4: *"A failed
/// mirror write does not reject the entry"* — and a `DomainError` here would
/// invite a later edit to propagate it with a `?`, turning an accepted entry
/// into a rejected one. The type carries a message for the log and nothing
/// a caller could act on.
#[derive(Debug, Clone)]
pub struct MirrorError(String);

impl MirrorError {
    /// Wraps a backend failure's description.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reads and writes the gear's declaration mirror.
///
/// One row per resolved meter, keyed by identifier. Not tenant-scoped:
/// declarations are platform-global (DESIGN §3.7).
#[async_trait]
pub trait DeclarationMirror: Send + Sync + 'static {
    /// Writes `document` for `id` under its registry reference `type_uuid`,
    /// setting first-seen and the reference once and last-seen on every write.
    ///
    /// `type_uuid` is the reference `types-registry` issued for `id`. Nothing
    /// here couples the two: this port stores the pair it is given, and the
    /// caller is responsible for having taken both from one resolved schema.
    ///
    /// # Errors
    ///
    /// [`MirrorError`] on any backend failure. The caller **counts it and
    /// serves the operation anyway** (`uc_declaration_mirror_write_failures_total`,
    /// DESIGN §3.11.5); it never rejects.
    async fn upsert(
        &self,
        id: &MeterTypeId,
        type_uuid: Uuid,
        document: &Value,
    ) -> Result<(), MirrorError>;

    /// Reads `id`'s mirrored document, or `None` where no row exists.
    ///
    /// # Errors
    ///
    /// [`MirrorError`] on a backend failure.
    ///
    /// **A read failure is NOT counted on
    /// `uc_declaration_mirror_write_failures_total`** (spec ruling J6). That
    /// instrument's published meaning is *"A declaration resolved but not
    /// mirrored"*, which a read failure is not — the row may be perfectly
    /// present and the reader broken. Counting it there would tell an
    /// operator that types are failing to mirror when they are not. The
    /// caller logs it at `warn!` and treats it as "no row", so resolution
    /// fails closed.
    async fn read(&self, id: &MeterTypeId) -> Result<Option<Value>, MirrorError>;

    /// Resolves a registry reference back to the meter identifier it was
    /// issued for, or `None` where no row holds it.
    ///
    /// # Errors
    ///
    /// [`MirrorError`] on a backend failure, and on a row whose stored
    /// identifier no longer passes [`MeterTypeId::new`] — the same posture
    /// [`Self::read`] takes for a row whose document will not parse, and for
    /// the same reason: a caller cannot tell a defect from an absent row
    /// unless this says so.
    ///
    /// Both outcomes are treated alike by the caller, which falls through to
    /// `types-registry`: a corrupt cache row must not fail a resolution the
    /// authority can answer. Keeping them distinct here is what lets the
    /// caller log a defect as a defect rather than record an ordinary miss.
    ///
    /// **A read failure is NOT counted on
    /// `uc_declaration_mirror_write_failures_total`**, for the reason
    /// [`Self::read`]'s own doc gives (spec ruling J6).
    async fn resolve_id(&self, type_uuid: Uuid) -> Result<Option<MeterTypeId>, MirrorError>;
}

/// [`DeclarationMirror`] for a gear with no mirror wired — the default
/// [`crate::domain::type_resolver::TypeResolver::new`] supplies, and the
/// pre-8b gear's behaviour exactly.
///
/// Mirrors [`super::declarations::UnavailableDeclarationSource`]'s role: a
/// domain-owned placeholder for contexts that need *a* implementation to
/// construct a resolver but have no real adapter to give it. Production has
/// one construction point and it uses
/// [`crate::domain::type_resolver::TypeResolver::with_rehydration`] (Task 5
/// wired it; this was plain text, not a link, until that task landed).
///
/// **No-op, NOT failing, and the distinction is load-bearing (controller
/// ruling J18).** A failing default would make every existing driving
/// scenario in `service_metrics_tests` attempt a mirror write, fail it, and
/// increment `uc_declaration_mirror_write_failures_total` — emitting that
/// counter by way of a universally broken mirror, which would turn the
/// §3.11.5 inventory pin green on a vacuous emission and add a spurious
/// failure count and a `warn!` to dozens of unrelated tests.
///
/// - `upsert` answers `Ok(())`, writing nowhere. ADR statement 4 makes a
///   failed write indistinguishable from a successful one to a caller, so the
///   only difference a failing variant would buy is the false counter above.
/// - `read` answers `Ok(None)` — "no row", which is the **fail-closed**
///   outcome `inst-restore-read` prescribes, so a restore against this double
///   correctly refuses rather than silently succeeding.
///
/// **The cost of that no-op posture is that the unwired state has no
/// telemetric signal of its own, so `upsert` emits one in a log.** If a
/// future production path ever reached
/// [`crate::domain::type_resolver::TypeResolver::new`], every observable
/// would look healthy: `upsert`'s `Ok(())` leaves
/// `uc_declaration_mirror_write_failures_total` at zero (indistinguishable
/// from a mirror that is writing fine), `read`'s `Ok(None)` makes every
/// restore refuse and count `Unresolved` (indistinguishable from "mirrored
/// nowhere"), and the gear's own startup checks pass regardless —
/// `ctx.db_required()` still succeeds and the migration still creates the
/// table, empty forever. The `debug!` below names this type, so the unwired
/// state is at least greppable in a log.
///
/// Deliberately **not** `#[cfg(test)]`-gated: `new` has 23
/// production-adjacent call sites across this gear's tests, and gating it
/// would be a larger change than the signal is worth.
pub struct NoopDeclarationMirror;

#[async_trait]
impl DeclarationMirror for NoopDeclarationMirror {
    async fn upsert(
        &self,
        id: &MeterTypeId,
        _type_uuid: Uuid,
        _document: &Value,
    ) -> Result<(), MirrorError> {
        // `debug!`, not `warn!`: every pre-8b `TypeResolver::new` call site
        // in this gear's own tests hits this, and the condition is a
        // construction fact rather than a runtime failure. See this type's
        // own doc for why the no-op has no other signal at all.
        tracing::debug!(
            gts_type_id = %id,
            "NoopDeclarationMirror: no declaration mirror is wired, so this \
             declaration is written nowhere and will not be restorable after a \
             types-registry restart"
        );
        Ok(())
    }

    async fn read(&self, _id: &MeterTypeId) -> Result<Option<Value>, MirrorError> {
        Ok(None)
    }

    async fn resolve_id(&self, _type_uuid: Uuid) -> Result<Option<MeterTypeId>, MirrorError> {
        Ok(None)
    }
}
