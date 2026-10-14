//! The pipeline's invalidation-target stage: resolve each distinct
//! `(target, lookup scope)` reference once, then check every withdrawal
//! naming it against the entry it found.
//!
//! The fan-out is bounded by [`super::TARGET_LOOKUP_FANOUT_CONCURRENCY`].
//! [`verify_pending_target`] holds the per-withdrawal semantics: the
//! faithful-copy comparator, the metadata rules and the same-batch verdict.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use futures::stream;
use toolkit_odata::ast;
use usage_collector_sdk::v2::{
    EntryType, UsageCollectorError, UsageCollectorPluginError, UsageCollectorPluginV2,
};
use uuid::Uuid;

use super::projection::observe_metadata_bytes;
use super::{
    DeclarationCache, InvalidationTargetCache, PendingTarget, Slot,
    TARGET_LOOKUP_FANOUT_CONCURRENCY, Withdrawal, validate_submit_record_metadata,
};
use crate::v2::domain::error;
use crate::v2::domain::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_domain_error,
};
use crate::v2::domain::invalidation::verify_invalidation_target;
use crate::v2::domain::observability;
use crate::v2::domain::ports::metrics::PluginOp;
use crate::v2::domain::service::Service;

impl Service {
    /// Resolve every deferred invalidation-target check the declaration stage
    /// left behind.
    ///
    /// Builds a request-local `Map<(target, permit scope), Result<UsageRecord,
    /// _>>` via a bounded `get_usage_record` fan-out over the **distinct**
    /// `(target, lookup scope id)` pairs — a batch withdrawing one target twice
    /// under one permit costs one read, not two (`inst-algo-semantics-l1-dedup` /
    /// `inst-algo-semantics-l1-bounded-fanout`) — then, for every
    /// [`Slot::AwaitingTarget`], runs [`verify_invalidation_target`] and the
    /// deferred metadata check, advancing the slot to `Resolved` (rejection) or
    /// `Eligible` (verified).
    ///
    /// The metadata check stays behind the target check so a submission breaking
    /// both is told about the copy: the metadata it would be told to fix is
    /// metadata it has to copy from the target regardless.
    ///
    /// **Pairing a row to its entry is this function's obligation, in both
    /// directions**, since [`verify_invalidation_target`] takes the row it is
    /// handed and never re-checks that it is the row the entry named.
    ///
    /// *Request-local:* the fan-out key and the slot are read off one
    /// destructured [`PendingTarget`], which travels in the very slot its outcome
    /// lands in, so no entry is verified against a row fetched for a different
    /// entry and no outcome lands on a different input index.
    ///
    /// *Store-side:* the returned row's `id` must equal the id it was fetched for.
    /// This cannot be left to the SPI contract: a submission that happens to be a
    /// faithful copy of the *returned* row would be **accepted**, withdrawing an
    /// entry nothing ever checked.
    ///
    /// At-most-one-invalidation is not checked here: every withdrawal of one record
    /// reaches the same six identity inputs, so a second collides on the dedup
    /// identity at the store and is lifted at dispatch ([`lift_dispatch_error`]).
    ///
    /// **Traceability.** The entry-type guard below is
    /// `cpt-cf-usage-collector-dod-no-invalidation-of-invalidation`'s second half
    /// on the batch path: a plugin returning an invalidation under that identifier
    /// is a contract breach and surfaces as an internal failure rather than a
    /// caller error. Its first half holds by construction, through
    /// [`derive_invalidation_target`]'s fixed `entry_type = record`.
    // @cpt-algo:cpt-cf-usage-collector-algo-target-resolution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-no-invalidation-of-invalidation:p1
    // @cpt-begin:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-dedup
    // @cpt-begin:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-bounded-fanout
    // @cpt-begin:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-lookup
    pub(super) async fn resolve_invalidation_targets(
        &self,
        slots: Vec<Slot>,
        plugin: &dyn UsageCollectorPluginV2,
        declaration_cache: &DeclarationCache,
        batch_ids: &HashSet<Uuid>,
    ) -> Vec<Slot> {
        let metrics = self.metrics.as_ref();
        let distinct_lookups: HashMap<(Uuid, usize), Arc<ast::Expr>> = slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::AwaitingTarget(pending) => Some((
                    (
                        pending.withdrawal.invalidation.target,
                        pending.lookup_scope_id,
                    ),
                    Arc::clone(&pending.lookup_scope),
                )),
                _ => None,
            })
            .collect();

        if distinct_lookups.is_empty() {
            return slots;
        }

        let target_cache: InvalidationTargetCache =
            stream::iter(distinct_lookups.into_iter().map(|(key, scope)| async move {
                let (target, _) = key;
                let outcome = observability::instrument_spi(
                    metrics,
                    PluginOp::GetUsageRecord,
                    plugin.get_usage_record(target, &scope, true),
                )
                .await
                .map_err(|e| match e {
                    UsageCollectorPluginError::UsageRecordNotConverged { .. } => {
                        DomainError::TargetNotConverged { target }
                    }
                    other => DomainError::from(other),
                });
                (key, outcome)
            }))
            .buffer_unordered(TARGET_LOOKUP_FANOUT_CONCURRENCY)
            .collect()
            .await;

        slots
            .into_iter()
            .map(|slot| match slot {
                Slot::AwaitingTarget(pending) => {
                    self.verify_pending_target(pending, &target_cache, declaration_cache, batch_ids)
                }
                other => other,
            })
            .collect()
    }
    // @cpt-end:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-lookup
    // @cpt-end:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-bounded-fanout
    // @cpt-end:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-dedup

    /// Settle one pending withdrawal against the row the target pre-pass read for
    /// it: the store-side contract checks, [`verify_invalidation_target`], then
    /// the deferred metadata check, in that order.
    fn verify_pending_target(
        &self,
        pending: PendingTarget,
        target_cache: &InvalidationTargetCache,
        declaration_cache: &DeclarationCache,
        batch_ids: &HashSet<Uuid>,
    ) -> Slot {
        let metrics = self.metrics.as_ref();
        let metadata_size_cap_bytes = self.metadata_size_cap_bytes;
        let unavailable_retry_after_secs = self.unavailable_retry_after_secs;
        let target_not_converged_retry_after_secs = self.target_not_converged_retry_after_secs;
        let PendingTarget {
            withdrawal:
                Withdrawal {
                    submission,
                    invalidation,
                },
            meter,
            record,
            lookup_scope: _,
            lookup_scope_id,
        } = pending;

        // @cpt-begin:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-not-found
        // The pre-pass populates the cache for every pending target, so a
        // missing entry here is a host-invariant breach: a typed `Internal`
        // per-record error, never `unreachable!()` — request paths must not
        // panic on an invariant failure.
        let target = match target_cache.get(&(invalidation.target, lookup_scope_id)) {
            Some(Ok(row)) => row,
            Some(Err(DomainError::UsageRecordNotFound { .. })) => {
                return Slot::Resolved(Err(if batch_ids.contains(&invalidation.target) {
                    // Not a realization site for `inst-err-not-converged`: the
                    // plugin answered `UsageRecordNotFound`, and this is the
                    // host's own same-batch inference reusing the conflict
                    // shape and its configured delay.
                    UsageCollectorError::target_not_converged(
                        invalidation.target,
                        Some(target_not_converged_retry_after_secs),
                    )
                } else {
                    UsageCollectorError::invalidation_target_not_found(invalidation.target)
                }));
            }
            Some(Err(e)) => {
                return Slot::Resolved(Err(lift_domain_error(
                    e.clone(),
                    UnavailableRetryAfterSecs(unavailable_retry_after_secs),
                    TargetNotConvergedRetryAfterSecs(target_not_converged_retry_after_secs),
                )));
            }
            None => {
                return Slot::Resolved(Err(error::invariant_breach(format!(
                    "target pre-pass cache miss for invalidates {}",
                    invalidation.target,
                ))));
            }
        };
        // @cpt-end:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-not-found

        // The store answered with a different entry than the one it was
        // asked for. A host-invariant breach, not a caller fault: the id
        // was the gateway's to supply and the SPI's to honour. The detail
        // names only the id the caller sent — the row's own identity is
        // exactly what must not cross back on an unscoped read.
        if target.id != invalidation.target {
            return Slot::Resolved(Err(error::invariant_breach(format!(
                "storage plugin answered get_usage_record({}) with a different entry",
                invalidation.target,
            ))));
        }

        // A row answering an identifier derived with `entry_type = record`
        // cannot be an invalidation; one that is contradicts its own
        // identifier. Same class of store breach as the id mismatch above,
        // refused rather than accepted as a withdrawal of a withdrawal.
        if target.entry_type() == EntryType::Invalidation {
            return Slot::Resolved(Err(error::invariant_breach(format!(
                "storage plugin answered get_usage_record({}) with an invalidation",
                invalidation.target,
            ))));
        }

        // The comparator is handed the submission, not `record`: the two
        // carry the same caller-supplied fields, but only the submission
        // shape makes a field added to it alone a compile error there.
        if let Err(e) = verify_invalidation_target(&submission, meter.uuid, &invalidation, target) {
            return Slot::Resolved(Err(e));
        }

        // Metadata check deferred behind the target check, per the
        // error-priority ordering. A missing declaration entry here is a
        // host-invariant breach (the pre-pass covers every PDP-allowed
        // record's gts_type_id); typed `Internal`, never a panic.
        let Some(Ok(declaration)) = declaration_cache.get(&record.gts_type_id) else {
            return Slot::Resolved(Err(error::invariant_breach(format!(
                "declaration pre-pass cache miss for gts_type_id {} before the metadata check",
                record.gts_type_id,
            ))));
        };
        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
        observe_metadata_bytes(metrics, &record.metadata);
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
        if let Err(e) =
            validate_submit_record_metadata(declaration, &record.metadata, metadata_size_cap_bytes)
        {
            return Slot::Resolved(Err(e));
        }

        Slot::Eligible { meter, record }
    }
}
