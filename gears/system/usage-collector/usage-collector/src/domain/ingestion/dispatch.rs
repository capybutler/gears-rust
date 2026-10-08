//! The ingestion pipeline's SPI fan-out: one dispatch per dedup identity,
//! and the projection of each outcome onto every input index sharing that
//! identity (DESIGN §3.1 "Collision resolution").
//!
//! [`Service::dispatch_eligible_entries`] sends the representative of each
//! identity; [`settle_dispatched`] keeps what that call decided as a
//! [`Resolution`], and [`resolve_follower`] answers every later entry of the
//! same identity against it.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use usage_collector_sdk::{
    Invalidation, MeterRef, StoredUsageRecord, UsageCollectorError, UsageCollectorPluginError,
    UsageCollectorPluginV1, UsageRecord,
};
use uuid::Uuid;

use crate::domain::error;
use crate::domain::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_dispatch_error,
    lift_domain_error,
};
use crate::domain::observability;
use crate::domain::ports::metrics::PluginOp;
use crate::domain::service::Service;

/// What a later same-identity entry of a batch resolves against, once the first
/// entry of its identity has been dispatched (DESIGN §3.1 "Collision
/// resolution").
#[allow(
    clippy::large_enum_variant,
    reason = "Against carries the stored UsageRecord itself so a follower can be compared and returned without a second allocation-hiding indirection; the enum lives only for the span of one batch dispatch, never stored or cloned in bulk"
)]
enum Resolution {
    /// The entry the store holds for the identity: the first entry's accepted
    /// row, or the stored entry its `IdempotencyConflict` carried.
    Against(UsageRecord),
    /// The first entry failed for a reason that holds no stored entry; a later
    /// entry is told the same.
    Failed(DomainError),
}

/// Lift one dispatched entry's plugin outcome, and keep what a later entry of
/// the same identity resolves against.
///
/// Together with [`resolve_follower`], realizes
/// `cpt-cf-usage-collector-algo-idempotency-outcome` /
/// `cpt-cf-usage-collector-dod-idempotency-outcomes` /
/// `cpt-cf-usage-collector-flow-retry-ingestion-submission`: an
/// `IdempotencyConflict` from the plugin is lifted into both the caller's error
/// and the stored entry a later same-identity entry of the batch resolves
/// against, so a resubmission is absorbed (returning the stored entry with its
/// original `accepted_at` / `origin`) or rejected as a conflict — never exposed
/// as a third "duplicate" outcome.
///
/// **Both arms re-attach, and both check the reference first.** The entry a
/// plugin answers with names its meter by reference alone, and the identifier
/// stapled back on is the *gear's*, so a row under another meter's reference
/// would otherwise be relabelled and served as fact.
/// [`reattach`](crate::domain::list_query::reattach) is the same check where
/// a bare `Result` suffices; this one needs a [`Resolution`] beside it, so
/// the comparison is spelled out rather than called.
///
/// **Traceability.** With [`lift_dispatch_error`] and [`resolve_follower`] this
/// is `cpt-cf-usage-collector-algo-invalidation-collision-outcome` and
/// `cpt-cf-usage-collector-dod-single-invalidation-per-entry`, whose four
/// outcomes map one to one onto the arms below:
///
/// * *persisted* — `Ok(stored)`.
/// * *absorbed* — the same arm, reached through a store that returned the entry
///   it already held, carrying that entry's `accepted_at` and `origin`.
/// * *collision on a dispatched invalidation* —
///   `Conflict(AlreadyInvalidated)` naming the target, the accepted
///   invalidation and its stored reason code; or `Internal` where the stored
///   entry carries no invalidation, which the dedup identity makes a plugin
///   contract breach.
/// * *transient* — `ServiceUnavailable`, never reported as a collision.
///
/// At-most-one-invalidation needs no rule here: every withdrawal of one record
/// repeats that record's five shared identity components and reads
/// `entry_type = invalidation`, so all reach one dedup identity and the second
/// collides (DESIGN §3.1, "At most one invalidation").
// @cpt-algo:cpt-cf-usage-collector-algo-idempotency-outcome:p1
// @cpt-algo:cpt-cf-usage-collector-algo-invalidation-collision-outcome:p1
// @cpt-dod:cpt-cf-usage-collector-dod-idempotency-outcomes:p1
// @cpt-dod:cpt-cf-usage-collector-dod-single-invalidation-per-entry:p1
// @cpt-flow:cpt-cf-usage-collector-flow-retry-ingestion-submission:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn settle_dispatched(
    outcome: Result<StoredUsageRecord, UsageCollectorPluginError>,
    meter: &MeterRef,
    invalidation: Option<&Invalidation>,
    unavailable_retry_after_secs: u64,
    target_not_converged_retry_after_secs: u64,
) -> (Result<UsageRecord, UsageCollectorError>, Resolution) {
    // A record under another meter's reference, on either arm. Reported to
    // this entry's caller and kept as the resolution every later entry of
    // the same identity is told, so one breach is one answer rather than a
    // breach for the representative and a silent success for its followers.
    let foreign = |stored_type_uuid: Uuid| {
        let detail = error::foreign_reference_detail(stored_type_uuid, meter);
        (
            Err(error::invariant_breach(detail.clone())),
            Resolution::Failed(DomainError::Internal(detail)),
        )
    };
    match outcome {
        Ok(stored) if stored.gts_type_uuid != meter.uuid => foreign(stored.gts_type_uuid),
        Ok(stored) => {
            let record = stored.into_usage_record(meter.id.clone());
            (Ok(record.clone()), Resolution::Against(record))
        }
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. })
            if existing.gts_type_uuid != meter.uuid =>
        {
            foreign(existing.gts_type_uuid)
        }
        Err(UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing,
        }) => {
            let stored = (*existing).clone().into_usage_record(meter.id.clone());
            let lifted = lift_dispatch_error(
                UsageCollectorPluginError::IdempotencyConflict {
                    idempotency_key,
                    existing,
                },
                invalidation,
            );
            (
                Err(lift_domain_error(
                    lifted,
                    UnavailableRetryAfterSecs(unavailable_retry_after_secs),
                    TargetNotConvergedRetryAfterSecs(target_not_converged_retry_after_secs),
                )),
                Resolution::Against(stored),
            )
        }
        Err(other) => {
            let lifted = lift_dispatch_error(other, invalidation);
            (
                Err(lift_domain_error(
                    lifted.clone(),
                    UnavailableRetryAfterSecs(unavailable_retry_after_secs),
                    TargetNotConvergedRetryAfterSecs(target_not_converged_retry_after_secs),
                )),
                Resolution::Failed(lifted),
            )
        }
    }
}

/// Resolve a later same-identity entry the way the store would have: absorbed
/// into the stored entry when its caller-supplied fields are equal, a conflict
/// lifted by its own kind otherwise.
///
/// `meter` is the follower's own, and it is necessarily the representative's
/// too: the two share a derived `id`, whose six inputs include the meter.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn resolve_follower(
    entry: &UsageRecord,
    meter: &MeterRef,
    resolution: &Resolution,
    unavailable_retry_after_secs: u64,
    target_not_converged_retry_after_secs: u64,
) -> Result<UsageRecord, UsageCollectorError> {
    match resolution {
        Resolution::Against(stored) if stored.caller_supplied_eq(entry) => Ok(stored.clone()),
        Resolution::Against(stored) => Err(lift_domain_error(
            lift_dispatch_error(
                UsageCollectorPluginError::idempotency_conflict(
                    entry.idempotency_key.as_str(),
                    // Back into the shape the conflict carries. The
                    // representative's own settlement already checked this
                    // reference against the meter it dispatched under, and a
                    // follower shares that identity — so this re-attaches the
                    // same meter rather than naming a new one.
                    stored.clone().into_stored(meter.uuid),
                ),
                entry.invalidation.as_ref(),
            ),
            UnavailableRetryAfterSecs(unavailable_retry_after_secs),
            TargetNotConvergedRetryAfterSecs(target_not_converged_retry_after_secs),
        )),
        Resolution::Failed(error) => Err(lift_domain_error(
            error.clone(),
            UnavailableRetryAfterSecs(unavailable_retry_after_secs),
            TargetNotConvergedRetryAfterSecs(target_not_converged_retry_after_secs),
        )),
    }
}

impl Service {
    /// Dispatch the batch's eligible entries, one per dedup identity (DESIGN §3.1
    /// "Collision resolution"): `eligible` is in input
    /// order, so the first entry of each identity is the earliest and is the one
    /// sent to the plugin; later entries of the same identity resolve against
    /// what the store holds for it once the representative's outcome is known.
    ///
    /// The representatives go to the plugin in one `create_usage_records` call.
    /// Each outcome is lifted onto its input slot by [`settle_dispatched`], which
    /// also keeps the stored entry (or the failure) its identity resolves to;
    /// each follower is then answered by [`resolve_follower`] against that
    /// resolution. `Err` is returned only when the call fails as a whole or the
    /// plugin answers a different number of outcomes than it was sent.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub(super) async fn dispatch_eligible_entries(
        &self,
        plugin: &dyn UsageCollectorPluginV1,
        eligible: Vec<(usize, MeterRef, UsageRecord)>,
        results: &mut [Option<Result<UsageRecord, UsageCollectorError>>],
    ) -> Result<(), UsageCollectorError> {
        let metrics = self.metrics.as_ref();
        let unavailable_retry_after_secs = self.unavailable_retry_after_secs;
        let target_not_converged_retry_after_secs = self.target_not_converged_retry_after_secs;
        let mut representative_of: HashMap<Uuid, usize> = HashMap::new();
        let mut representatives: Vec<(usize, MeterRef, UsageRecord)> = Vec::new();
        let mut followers: Vec<(usize, MeterRef, UsageRecord)> = Vec::new();
        for (index, meter, record) in eligible {
            match representative_of.entry(record.id) {
                Entry::Occupied(_) => followers.push((index, meter, record)),
                Entry::Vacant(slot) => {
                    slot.insert(representatives.len());
                    representatives.push((index, meter, record));
                }
            }
        }

        if representatives.is_empty() {
            return Ok(());
        }

        let dispatched_invalidations: Vec<Option<Invalidation>> = representatives
            .iter()
            .map(|(_, _, record)| record.invalidation.clone())
            .collect();
        // Each entry crosses the SPI paired with the meter it was resolved
        // under, and comes back to be settled against that same pair — so the
        // reference a returned record carries is checked against the reference
        // the gear dispatched, never against whichever meter happened to be
        // last in scope.
        let mut indices: Vec<usize> = Vec::with_capacity(representatives.len());
        let mut meters: Vec<MeterRef> = Vec::with_capacity(representatives.len());
        let mut dispatched: Vec<(MeterRef, StoredUsageRecord)> =
            Vec::with_capacity(representatives.len());
        for (index, meter, record) in representatives {
            indices.push(index);
            dispatched.push((meter.clone(), record.into_stored(meter.uuid)));
            meters.push(meter);
        }
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-dispatch
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-spi-catch
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-spi-fail-mark
        let spi_results = observability::instrument_spi(
            metrics,
            PluginOp::CreateUsageRecords,
            plugin.create_usage_records(dispatched),
        )
        .await
        .map_err(|e| {
            lift_domain_error(
                DomainError::from(e),
                UnavailableRetryAfterSecs(unavailable_retry_after_secs),
                TargetNotConvergedRetryAfterSecs(target_not_converged_retry_after_secs),
            )
        })?;
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-spi-fail-mark
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-spi-catch
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-dispatch

        if spi_results.len() != indices.len() {
            return Err(error::invariant_breach(format!(
                "plugin returned {} per-record results for {} dispatched records",
                spi_results.len(),
                indices.len()
            )));
        }

        let mut resolutions: Vec<Resolution> = Vec::with_capacity(indices.len());
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-outcome
        for (((index, meter), spi_result), invalidation) in indices
            .into_iter()
            .zip(meters)
            .zip(spi_results)
            .zip(dispatched_invalidations)
        {
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-accepted
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-conflict
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-spi-err
            let (outcome, resolution) = settle_dispatched(
                spi_result,
                &meter,
                invalidation.as_ref(),
                unavailable_retry_after_secs,
                target_not_converged_retry_after_secs,
            );
            results[index] = Some(outcome);
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-spi-err
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-conflict
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-accepted
            resolutions.push(resolution);
        }
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-outcome

        for (index, meter, record) in followers {
            results[index] = Some(
                match representative_of
                    .get(&record.id)
                    .and_then(|position| resolutions.get(*position))
                {
                    Some(resolution) => resolve_follower(
                        &record,
                        &meter,
                        resolution,
                        unavailable_retry_after_secs,
                        target_not_converged_retry_after_secs,
                    ),
                    None => Err(error::invariant_breach(format!(
                        "no dispatched entry resolved identity {} of input {index}",
                        record.id
                    ))),
                },
            );
        }

        Ok(())
    }
}
