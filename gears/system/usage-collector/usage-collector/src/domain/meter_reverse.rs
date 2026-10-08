//! Reverse resolution: a `types-registry` registry reference back to the meter
//! identifier it was issued for.
//!
//! The forward direction lives in [`crate::domain::type_resolver`], which
//! resolves a `MeterTypeId` to its declaration. This is its counterpart, and it
//! exists because one Plugin SPI method — `get_usage_record` — takes no type at
//! all, so a storage backend that keys entries on the reference cannot name the
//! meter in the record it returns.
//!
//! # Why `types-registry` is last and not first
//!
//! `cpt-cf-types-registry-adr-storage-identity-query-model` guarantees that one
//! identifier maps to one reference for the life of the installation, across
//! tenants, processes, deployments, imports and restores. A cached pair
//! therefore cannot go stale, because neither side of it can change — the mirror
//! can *miss*, it cannot *lie*. That is what makes a cache sound here at all,
//! and why the authority is consulted only on a miss.
//!
//! It is also why the in-memory map takes no TTL.
//! [`crate::domain::type_resolver::TypeResolver`] ages its entries because fold,
//! canonical unit and metadata surface can change under a re-authored
//! declaration; this mapping cannot. It takes no eviction policy either: it holds
//! one entry per distinct meter ever reverse-resolved, the same order as the
//! mirror table itself.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde_json::Value;
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::declaration_mirror::DeclarationMirror;
use crate::domain::ports::declarations::DeclarationSource;
use crate::domain::ports::metrics::UsageCollectorMetrics;

/// Bounds the mirror write below. This write runs on the request path
/// (`MeterReverseResolver::resolve`), the same kind of path
/// [`crate::domain::type_resolver`]'s `mirror_document` guards with the
/// identical value — matched here rather than independently chosen, so a
/// saturated mirror database costs the same bounded delay on either
/// direction.
const MIRROR_WRITE_TIMEOUT: Duration = Duration::from_millis(50);

/// Resolves a registry reference to its meter identifier.
pub struct MeterReverseResolver {
    source: Arc<dyn DeclarationSource>,
    mirror: Arc<dyn DeclarationMirror>,
    metrics: Arc<dyn UsageCollectorMetrics>,
    known: RwLock<HashMap<Uuid, MeterTypeId>>,
}

impl MeterReverseResolver {
    /// Builds a resolver over `source` as the authority and `mirror` as its
    /// durable cache tier.
    #[must_use]
    pub fn new(
        source: Arc<dyn DeclarationSource>,
        mirror: Arc<dyn DeclarationMirror>,
        metrics: Arc<dyn UsageCollectorMetrics>,
    ) -> Self {
        Self {
            source,
            mirror,
            metrics,
            known: RwLock::new(HashMap::new()),
        }
    }

    /// The meter identifier `type_uuid` was issued for.
    ///
    /// # Errors
    ///
    /// - [`DomainError::DeclarationNotFound`] when `types-registry` gives a
    ///   definite not-found answer, and when it answers with a type that is
    ///   not a meter. Both are conclusive facts about the reference rather
    ///   than transport failures, so neither is cached.
    /// - [`DomainError::TypesRegistryUnavailable`] otherwise.
    pub async fn resolve(&self, type_uuid: Uuid) -> Result<MeterTypeId, DomainError> {
        if let Some(id) = self.cached(type_uuid) {
            return Ok(id);
        }

        match self.mirror.resolve_id(type_uuid).await {
            Ok(Some(id)) => {
                self.remember(type_uuid, &id);
                return Ok(id);
            }
            Ok(None) => {}
            // Not counted on `uc_declaration_mirror_write_failures_total`:
            // that instrument means "a declaration resolved but not
            // mirrored", which a read failure is not. The
            // resolve continues against the authority — a corrupt or broken
            // cache must not fail a question the registry can answer.
            Err(e) => tracing::warn!(
                %type_uuid,
                error = %e,
                "declaration mirror reverse read failed; falling through to types-registry"
            ),
        }

        let schema = self.source.fetch_by_uuid(type_uuid).await?;

        let id = MeterTypeId::new(schema.type_id.as_ref()).map_err(|e| {
            // Logged, never put on the wire: `DomainError::DeclarationNotFound`'s
            // `From` impl (`error.rs`) builds the 404 `detail` straight out of
            // `reason`, so naming `schema.type_id` there would let a caller who
            // merely supplies a UUID learn the identity of a GTS type it does not
            // own — including one belonging to another gear. It is logged at
            // `error` rather than `warn`, unlike the mirror arms above: with
            // `type_uuid` fed from this gear's own ledger, landing here means
            // gear-side data corruption.
            tracing::error!(
                %type_uuid,
                resolved_type_id = %schema.type_id.as_ref(),
                error = %e,
                "registry reference resolves to a type that is not a usage-collector meter type"
            );
            DomainError::DeclarationNotFound {
                gts_type_id: type_uuid.to_string(),
                reason: "does not name a usage-collector meter type".to_owned(),
            }
        })?;

        // The DoD requires the row be rewritten "on every successful registry
        // read, cold miss and refresh alike"; this is one. Failure is counted and
        // swallowed, per ADR-0015 statement 4.
        //
        // Writes `type_uuid` (the parameter this call resolved), not
        // `schema.type_uuid`. `DeclarationSource::fetch_by_uuid`'s contract says
        // the two agree for a resolvable reference, but this function's cache
        // key, its mirror miss lookup and its mirror write all need to agree with
        // EACH OTHER regardless of that contract. Using one value for every use
        // site removes the question rather than relying on the two staying
        // equal.
        self.mirror_upsert(&id, type_uuid, &schema.raw_schema).await;

        self.remember(type_uuid, &id);
        Ok(id)
    }

    /// Writes `id`/`type_uuid`'s pair back to the mirror, bounded by
    /// `MIRROR_WRITE_TIMEOUT` exactly like the forward resolver's
    /// `type_resolver::mirror_document`. Erroring and timed-out writes are
    /// counted, logged and swallowed here — never propagated to `resolve`, so
    /// a saturated mirror database costs nothing beyond this bounded delay.
    async fn mirror_upsert(&self, id: &MeterTypeId, type_uuid: Uuid, document: &Value) {
        match tokio::time::timeout(
            MIRROR_WRITE_TIMEOUT,
            self.mirror.upsert(id, type_uuid, document),
        )
        .await
        {
            Ok(Ok(())) => {}
            // `path = "reverse"` is the one field that tells this log line
            // apart from `type_resolver::mirror_document`'s — otherwise
            // character-for-character identical — so an operator can tell a
            // forward mirror write failure from a reverse one.
            Ok(Err(e)) => {
                tracing::warn!(
                    gts_type_id = %id,
                    error = %e,
                    path = "reverse",
                    "declaration mirror write failed: this meter will not be \
                     restorable after a types-registry restart"
                );
                self.metrics.record_declaration_mirror_write_failure();
            }
            Err(_) => {
                tracing::warn!(
                    gts_type_id = %id,
                    timeout_ms = MIRROR_WRITE_TIMEOUT.as_millis(),
                    path = "reverse",
                    "declaration mirror write timed out: this meter will not be \
                     restorable after a types-registry restart"
                );
                self.metrics.record_declaration_mirror_write_failure();
            }
        }
    }

    fn cached(&self, type_uuid: Uuid) -> Option<MeterTypeId> {
        self.known.read().get(&type_uuid).cloned()
    }

    fn remember(&self, type_uuid: Uuid, id: &MeterTypeId) {
        self.known.write().insert(type_uuid, id.clone());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "meter_reverse_tests.rs"]
mod meter_reverse_tests;
