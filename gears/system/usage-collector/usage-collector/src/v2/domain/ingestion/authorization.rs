//! The pipeline's PDP stage.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream;
use time::OffsetDateTime;
use toolkit_security::SecurityContext;
use usage_collector_sdk::v2::{RecordOrigin, UsageCollectorError};

use super::{LookupScope, PDP_CONCURRENCY, PdpGroupDecision, Slot};
use crate::v2::domain::authz::{self, AttributionTupleKey};
use crate::v2::domain::covered_period::ingestion_action;
use crate::v2::domain::error::DomainError;
use crate::v2::domain::ports::metrics::PdpOp;
use crate::v2::domain::service::Service;

/// The PDP metrics operation label for an entry admitted by `origin`'s route.
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
            // Per record: one backfill batch can straddle the create/backfill window.
            let action = ingestion_action(covered_period_bounds, origin, now, record.window_end);
            distinct_tuples
                .entry(AttributionTupleKey::from_record(record, action))
                .or_default()
                .push(index);
        }
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-pdp

        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-pdp
        // Authorize tuple keys, not records, so deduped records share one PDP request.
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
                    for index in indices {
                        slots[index] = Slot::Resolved(Err(UsageCollectorError::from(e.clone())));
                    }
                }
            }
        }

        // Remaining admitted slots belong to permitted groups.
        slots
            .into_iter()
            .enumerate()
            .map(|(index, slot)| match slot {
                Slot::Admitted { record, withdrawal } => Slot::Authorized {
                    record,
                    // Fail closed if a withdrawal somehow missed its compiled scope.
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
