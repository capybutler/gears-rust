//! The pipeline's catalog stage.

use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream;
use usage_collector_sdk::v2::{MeterRef, MeterTypeId, UsageCollectorError, UsageRecord};

use super::projection::observe_metadata_bytes;
use super::{
    DeclarationCache, LookupScope, PendingTarget, Slot, TYPE_RESOLUTION_FANOUT_CONCURRENCY,
    Withdrawal, validate_submit_record_metadata,
};
use crate::v2::domain::error;
use crate::v2::domain::ports::metrics::UsageCollectorMetrics;
use crate::v2::domain::service::Service;

impl Service {
    /// The batch's declaration stage: resolve one declaration per
    /// **distinct** `gts_type_id`, then settle each authorized slot against
    /// it.
    ///
    /// The returned [`DeclarationCache`] outlives this stage because the
    /// target stage runs the deferred metadata check against the same
    /// declarations.
    pub(super) async fn resolve_declarations(
        &self,
        slots: Vec<Slot>,
    ) -> (Vec<Slot>, DeclarationCache) {
        let type_resolver = self.type_resolver.as_ref();
        let metrics = self.metrics.as_ref();
        let metadata_size_cap_bytes = self.metadata_size_cap_bytes;
        let distinct_gts_type_ids: HashSet<MeterTypeId> = slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Authorized { record, .. } => Some(record.gts_type_id.clone()),
                _ => None,
            })
            .collect();

        // Resolve each distinct type once, then project the result to all slots.
        let declaration_cache: DeclarationCache = stream::iter(
            distinct_gts_type_ids
                .into_iter()
                .map(|gts_type_id| async move {
                    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-resolve
                    let outcome = type_resolver.resolve(&gts_type_id).await;
                    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-resolve
                    (gts_type_id, outcome)
                }),
        )
        .buffer_unordered(TYPE_RESOLUTION_FANOUT_CONCURRENCY)
        .collect()
        .await;

        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-foreach
        let slots = slots
            .into_iter()
            .map(|slot| match slot {
                Slot::Authorized { record, withdrawal } => settle_declaration(
                    record,
                    withdrawal,
                    &declaration_cache,
                    metrics,
                    metadata_size_cap_bytes,
                ),
                // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-pdp
                // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-deny
                other => other,
                // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-deny
                // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-pdp
            })
            .collect();
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-foreach

        (slots, declaration_cache)
    }
}

/// Settle one authorized slot against the declaration pre-pass: a withdrawal
/// moves on to the target stage, an ordinary measurement takes the metadata
/// check here and becomes eligible for dispatch.
fn settle_declaration(
    record: UsageRecord,
    withdrawal: Option<(Withdrawal, LookupScope)>,
    declaration_cache: &DeclarationCache,
    metrics: &dyn UsageCollectorMetrics,
    metadata_size_cap_bytes: usize,
) -> Slot {
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-unknown-usage-type
    let declaration = match declaration_cache.get(&record.gts_type_id) {
        Some(Ok(decl)) => Arc::clone(decl),
        Some(Err(e)) => return Slot::Resolved(Err(UsageCollectorError::from(e.clone()))),
        // Typed error rather than panic if the pre-pass invariant is broken.
        None => {
            return Slot::Resolved(Err(error::invariant_breach(format!(
                "declaration pre-pass cache miss for gts_type_id {} during record dispatch",
                record.gts_type_id,
            ))));
        }
    };
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-unknown-usage-type

    let meter = MeterRef::new(declaration.type_uuid, record.gts_type_id.clone());

    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-semantics
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-semantics-invalid
    // Withdrawals defer metadata validation until after target lookup.
    if let Some((withdrawal, lookup_scope)) = withdrawal {
        let (lookup_scope_id, lookup_scope) = match lookup_scope {
            Ok(compiled) => compiled,
            Err(e) => return Slot::Resolved(Err(UsageCollectorError::from(e))),
        };
        return Slot::AwaitingTarget(PendingTarget {
            withdrawal,
            meter,
            record,
            lookup_scope,
            lookup_scope_id,
        });
    }
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-semantics-invalid
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-semantics

    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
    observe_metadata_bytes(metrics, &record.metadata);
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata-closed-shape
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata
    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata-too-large
    if let Err(e) =
        validate_submit_record_metadata(&declaration, &record.metadata, metadata_size_cap_bytes)
    {
        return Slot::Resolved(Err(e));
    }
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata-too-large
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata-closed-shape

    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-eligible
    Slot::Eligible { meter, record }
    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-eligible
}
