//! The pipeline's PDP stage: one authorization round-trip per distinct
//! attribution tuple, projected back onto every slot sharing that tuple.
//!
//! The fan-out is bounded by [`super::PDP_CONCURRENCY`]; [`pdp_op_for`] names
//! the PDP operation each ingestion route authorizes under.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream;
use time::OffsetDateTime;
use toolkit_security::SecurityContext;
use usage_collector_sdk::{RecordOrigin, UsageCollectorError};

use super::{LookupScope, PDP_CONCURRENCY, PdpGroupDecision, Slot};
use crate::domain::authz::{self, AttributionTupleKey};
use crate::domain::covered_period::ingestion_action;
use crate::domain::error::DomainError;
use crate::domain::ports::metrics::PdpOp;
use crate::domain::service::Service;

/// The `operation` label the PDP-helper instruments (`uc_pdp_*`,
/// `uc_authz_decisions_total`) carry for an entry admitted by `origin`'s
/// route.
///
/// The label follows the entry point; the verb an entry is authorized
/// against comes from [`ingestion_action`] and can disagree with it — see
/// [`PdpOp::Backfill`], which owns that distinction.
pub const fn pdp_op_for(origin: RecordOrigin) -> PdpOp {
    match origin {
        RecordOrigin::Live => PdpOp::Ingest,
        RecordOrigin::Backfill => PdpOp::Backfill,
    }
}

impl Service {
    /// The batch's authorization stage: one PDP decision per **distinct**
    /// attribution tuple, projected onto every slot that tuple covers.
    ///
    /// A permitted group that covers at least one withdrawal compiles its
    /// [`AccessScope`] into an `OData` filter **once** and hands the compiled
    /// [`LookupScope`] to each of its withdrawals — every entry of a group
    /// already shares one PDP decision, so it can share one compiled scope
    /// too. A group that authorizes no invalidation skips compilation
    /// entirely: an ordinary measurement never triggers a target lookup.
    ///
    /// Every `Admitted` slot advances exactly once: to `Authorized` under a
    /// permitted group, or to `Resolved(Err)` under a denied / unavailable
    /// one.
    pub(super) async fn authorize_slots(
        &self,
        mut slots: Vec<Slot>,
        ctx: &SecurityContext,
        origin: RecordOrigin,
        now: OffsetDateTime,
    ) -> Vec<Slot> {
        let covered_period_bounds = &self.covered_period_bounds;
        let enforcer = &self.enforcer;
        let metrics = self.metrics.as_ref();
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-authorize
        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-pdp
        let mut distinct_tuples: HashMap<AttributionTupleKey, Vec<usize>> = HashMap::new();
        for (index, slot) in slots.iter().enumerate() {
            let Slot::Admitted { record, .. } = slot else {
                continue;
            };
            // Per record, not per batch: `ingestion_action` reads each
            // entry's own `window_end`, so one backfill batch straddling
            // the configured window carries `create` and `backfill` at
            // once. `action` is part of `AttributionTupleKey`'s hash/eq,
            // which is what keeps two such entries sharing one attribution
            // tuple from collapsing onto a single PDP decision — the entry
            // beyond the window would otherwise ride in on the other's
            // `create` permit.
            let action = ingestion_action(covered_period_bounds, origin, now, record.window_end);
            distinct_tuples
                .entry(AttributionTupleKey::from_record(record, action))
                .or_default()
                .push(index);
        }
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-pdp

        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-pdp
        // The fan-out passes the tuple `key` (not a `&UsageRecord`) into the
        // PDP composer. This is the load-bearing safety property of the
        // dedup: `authorize_attribution_tuple` has no syntactic access to
        // any record field outside the key, so two records that
        // hash-equal under `AttributionTupleKey` cannot diverge in PDP
        // payload — they share the SAME `AccessRequest` by construction.
        // `action` is part of the key (hash/eq), which is what lets the
        // loop above vary it per record without two verbs collapsing onto
        // one decision.
        let pdp_decisions: Vec<PdpGroupDecision> = stream::iter(distinct_tuples.into_iter().map(
            |(key, indices)| async move {
                let decision = authz::authorize_attribution_tuple(
                    enforcer,
                    metrics,
                    pdp_op_for(origin),
                    ctx,
                    &key,
                )
                .await;
                (indices, decision)
            },
        ))
        .buffer_unordered(PDP_CONCURRENCY)
        .collect()
        .await;
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-pdp

        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-reject
        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-allow
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-pdp-projected-deny
        // Keyed by input index because that is what a group-to-entries fan-out
        // produces, and drained by the positional pass below: it is the stage's
        // own intermediate, never a second account of a slot's state.
        let mut lookup_scopes: HashMap<usize, LookupScope> = HashMap::new();
        let mut next_scope_id = 0_usize;
        for (indices, decision) in pdp_decisions {
            match decision {
                Ok(scope) => {
                    if !indices
                        .iter()
                        .any(|index| slots[*index].awaits_target_lookup())
                    {
                        continue;
                    }
                    let scope_id = next_scope_id;
                    next_scope_id += 1;
                    let compiled =
                        authz::scope_to_odata_filter(&scope).map(|expr| (scope_id, Arc::new(expr)));
                    for index in indices {
                        lookup_scopes.insert(index, compiled.clone());
                    }
                }
                Err(e) => {
                    // A PDP-transport failure (`AuthorizationUnavailable`) and a
                    // plugin `Transient` both lift to `ServiceUnavailable`; their
                    // curated `detail` strings keep them distinguishable for
                    // operator triage without a separate per-record origin tag.
                    for index in indices {
                        slots[index] = Slot::Resolved(Err(UsageCollectorError::from(e.clone())));
                    }
                }
            }
        }

        // Whatever is still `Admitted` belongs to a permitted group: the tuple
        // groups partition the admitted indices, and the loop above resolved
        // every refused group's slots outright.
        slots
            .into_iter()
            .enumerate()
            .map(|(index, slot)| match slot {
                Slot::Admitted { record, withdrawal } => Slot::Authorized {
                    record,
                    // The pairing this stage owes the next one, and the only place
                    // in the pipeline it is formed: a permitted group compiled a
                    // scope exactly when it covered a withdrawal, and a slot
                    // belongs to exactly one group, so the two are present
                    // together. The `unwrap_or_else` is how that is made
                    // *structurally* fail-closed rather than merely true
                    // (`cpt-cf-usage-collector-principle-fail-closed`): a
                    // withdrawal paired with nothing would otherwise lose its
                    // withdrawal half and be dispatched as an ordinary
                    // measurement, which is the one outcome a target check exists
                    // to prevent. Refused instead, at its own input index.
                    withdrawal: withdrawal.map(|withdrawal| {
                        let lookup_scope = lookup_scopes.remove(&index).unwrap_or_else(|| {
                        Err(DomainError::Internal(format!(
                            "the permitted tuple group covering input {index} compiled no target \
                             lookup scope for the withdrawal it carries"
                        )))
                    });
                        (withdrawal, lookup_scope)
                    }),
                },
                other => other,
            })
            .collect()
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-pdp-projected-deny
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-allow
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-reject
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-authorize
    }
}
