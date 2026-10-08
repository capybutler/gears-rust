//! The aggregate path's `group_by` declaredness check, and its orchestration.
//!
//! Split out of `domain/query.rs`, which keeps what both read paths use
//! ([`crate::domain::query`]); this module is the aggregate path's alone, and
//! holds:
//!
//! * `require_dimensions_declared` — checks a `group_by` list names only the
//!   fixed dimensions or a declared metadata key, each at most once. The
//!   aggregate path's alone: no other surface carries a `group_by`.
//! * [`Service::query_aggregated_usage_records`] — the read path itself,
//!   whose `impl Service` block lives at the foot of this module rather than
//!   in `domain/service.rs`, so the orchestration sits with the check it is
//!   assembled from.

use std::collections::{BTreeSet, HashSet};

use toolkit_odata::ODataQuery;
use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    AggregationDimension, AggregationResult, MAX_AGGREGATION_BUCKETS, MetadataFilter, MeterRef,
    MeterTypeId, TimeRange, UsageCollectorError,
};

use crate::domain::authz;
use crate::domain::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_domain_error,
};
use crate::domain::observability::{self, OperationLogEntry, log_operation_completed};
use crate::domain::ports::metrics::{PdpOp, PluginOp, QueryKind};
use crate::domain::query;
use crate::domain::query::{
    compose_query_with_scope, reject_off_label_literals, reject_unpublished_filter_fields,
    require_metadata_filter_keys_declared, require_metadata_filter_within_caps,
};
use crate::domain::service::Service;

/// Checks every `group_by` dimension is either a fixed field (no
/// declaration needed) or a metadata property `declared_keys` actually
/// declares, and that no dimension is named more than once.
///
/// `declared_keys` is read from the resolved declaration fresh for this
/// request (see [`crate::domain::type_resolver::CompiledMetadataSchema::declared_keys`]),
/// never cached independently of it — so a property declared a moment ago
/// is usable on the very next call, per Spec §3.11. `gts_type_id` is the
/// queried meter, carried onto the error for operator-log parity with
/// [`UsageCollectorError::unknown_metadata_key`].
///
/// There is no ceiling on how many dimensions a `group_by` may carry:
/// DESIGN's `AggregationDimension` row says arity is "never ... bounded by
/// a fixed ceiling on how many a request may carry", PRD §5.1 admits
/// dimensions "in any combination and any order", and the yaml's
/// `group_by` property carries `uniqueItems: true` and no `maxItems`. What
/// bounds the result is the aggregation result limit and the mandatory
/// time range, not dimension count — pinned by
/// `every_admissible_dimension_once_in_an_arbitrary_order_is_admitted` in
/// `query_tests.rs`.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] naming the first
/// undeclared metadata dimension, checked before uniqueness: a metadata key
/// differing only in case (e.g. `Region` vs `region`) is a typo rather than
/// a repeat, since declaredness is an exact `BTreeSet<String>` match. A
/// case-insensitive uniqueness pass run first would refuse that pair as a
/// duplicate and tell the caller to delete one when what they have is a
/// misspelling — the undeclared-key error is the actionable one and must
/// win. Once every dimension is declared, [`UsageCollectorError::InvalidArgument`]
/// (via [`UsageCollectorError::repeated_grouping_dimension`]) names the
/// first dimension that repeats an earlier one, by exact equality.
///
/// Traceability: the `group_by` half of
/// `cpt-cf-usage-collector-algo-query-field-validation` /
/// `cpt-cf-usage-collector-dod-query-field-validation` ("a grouping
/// dimension MUST be one of the five fixed dimensions or a declared
/// property, MUST appear at most once").
// @cpt-algo:cpt-cf-usage-collector-algo-query-field-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-query-field-validation:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub fn require_dimensions_declared(
    dimensions: &[AggregationDimension],
    declared_keys: &BTreeSet<String>,
    gts_type_id: &MeterTypeId,
) -> Result<(), UsageCollectorError> {
    // Declaredness first, and the order is load-bearing. Each refusal names
    // one dimension, so `[Region, region]` against a declaration carrying
    // only `region` has two available answers: "Region is undeclared" and
    // "these two repeat". The first is the caller's actual mistake — the
    // second would tell them to delete a duplicate when what they have is a
    // typo — and an undeclared name is wrong whatever its multiplicity.
    for dim in dimensions {
        if let AggregationDimension::Metadata(key) = dim
            && !declared_keys.contains(key.as_str())
        {
            return Err(UsageCollectorError::undeclared_metadata_dimension(
                gts_type_id,
                key.as_str(),
            ));
        }
    }

    // Each at most once (DESIGN §3.1's AggregationDimension row,
    // dod-query-field-validation, and the yaml's `uniqueItems: true`).
    // Exact equality, deliberately: declaredness above is an exact
    // `BTreeSet` match, so two spellings differing in case are two
    // different keys and one of them has already been refused as
    // undeclared. A case-insensitive comparison here would disagree with
    // the check above and refuse a typo as a duplicate.
    //
    // There is NO ceiling on how many dimensions a request carries —
    // DESIGN says "never by a fixed ceiling", and what bounds the result is
    // the aggregation result limit and the mandatory range. The absence is
    // pinned by `every_admissible_dimension_once_in_an_arbitrary_order_is_admitted`.
    let mut seen: HashSet<&AggregationDimension> = HashSet::new();
    for dim in dimensions {
        if !seen.insert(dim) {
            return Err(UsageCollectorError::repeated_grouping_dimension(
                gts_type_id,
                dim,
            ));
        }
    }
    Ok(())
}

impl Service {
    /// Aggregated read over `UsageRecord`s, narrowed by the PDP-returned
    /// constraints and executed server-side by the bound storage plugin.
    ///
    /// Mirrors [`Self::list_usage_records`] in posture. Five of that
    /// path's seven responsibilities live here per
    /// `cpt-cf-usage-collector-flow-query-aggregated-usage` — the
    /// keyset floor and the cursor binding do not, because this path
    /// returns buckets rather than a page and mints no continuation:
    ///
    /// 1. **Authorize** the request via [`authz::authorize_list_usage_records`]
    ///    (the PEP shape is shared: pre-row, no per-record attribution, with
    ///    `require_constraints(true)` so the PDP MUST return row-scope
    ///    narrowing). A constrained permit narrows the user filter; a
    ///    degenerate unconstrained permit is denied in composition by
    ///    [`authz::scope_to_odata_filter`], not left unscoped across tenants.
    /// 2. **Resolve** the queried meter's declaration through
    ///    [`TypeResolver::resolve`](crate::domain::type_resolver::TypeResolver::resolve),
    ///    fail-closed. There is no caller-chosen
    ///    aggregation: the fold served is exactly the one the declaration
    ///    names, so no request can name a different one. Runs before any
    ///    plugin dispatch, so an unresolvable type never reaches the SPI.
    /// 3. **Gate** the query surface on that declaration — all **five** Spec
    ///    §3.11 checks, the raw path's four plus `require_dimensions_declared`
    ///    on `group_by`, which only this path takes. Recomputed per request;
    ///    the rule lives in [`crate::domain::query`].
    /// 4. **Compose** the PDP constraints into the user-supplied `OData`
    ///    filter via [`compose_query_with_scope`]. The composition is
    ///    intersection-only per
    ///    `cpt-cf-usage-collector-algo-read-scope-composition`.
    ///    `time_range` is untouched by composition: it is a typed
    ///    parameter and never a `$filter` conjunct.
    /// 5. **Delegate** to the bound storage plugin's
    ///    `query_aggregated_usage_records` SPI with the composed filter,
    ///    the typed `gts_type_id`, the typed `time_range`, the metadata
    ///    side-channel, the declared
    ///    `usage_collector_sdk::AggregationFold`, and any `group_by`
    ///    dimensions, executed server-side per DESIGN §3.3 "Plugin SPI".
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::PermissionDenied`] /
    ///   [`UsageCollectorError::ServiceUnavailable`] when the PDP denies
    ///   or is unavailable, or when the PDP returns a constraint shape
    ///   this gear cannot honour (tree predicates on a flat resource,
    ///   unknown PEP property, type mismatch on a value).
    /// * [`UsageCollectorError::NotFound`] when the queried `gts_type_id` does
    ///   not resolve to a usable declaration.
    /// * Any other [`UsageCollectorError`] variant lifted from a plugin
    ///   transport / persistence failure.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub async fn query_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError> {
        let started = std::time::Instant::now();
        let log_referenced_type = gts_type_id.as_str().to_owned();
        let result = self
            .dispatch_aggregated_usage_records(
                ctx,
                gts_type_id,
                time_range,
                query,
                metadata_filter,
                group_by,
            )
            .await;

        // Enforce the declared aggregate-bucket cap (`AggregationResult.buckets`
        // `maxItems` in `usage-collector-v1.yaml`). The plugin bounds its own
        // scan to `MAX_AGGREGATION_BUCKETS + 1` rows, so an over-cap result
        // surfaces here as strictly more than the cap — rejected as the
        // client-fixable 400 it is, ahead of the result-row telemetry below so
        // it is classified as a client error rather than a page. Refused
        // rather than truncated: the aggregate half of
        // `dod-canonical-page-envelope`.
        // @cpt-dod:cpt-cf-usage-collector-dod-canonical-page-envelope:p1
        let result = result.and_then(|aggregation_result| {
            if aggregation_result.buckets.len() > MAX_AGGREGATION_BUCKETS {
                Err(UsageCollectorError::aggregation_result_too_large(
                    MAX_AGGREGATION_BUCKETS,
                ))
            } else {
                Ok(aggregation_result)
            }
        });
        let seconds = started.elapsed().as_secs_f64();
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-result-rows-observe
        if let Ok(aggregation_result) = &result {
            self.metrics.observe_query_result_rows(
                QueryKind::Aggregated,
                u64::try_from(aggregation_result.buckets.len()).unwrap_or(u64::MAX),
            );
        }
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-result-rows-observe
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-telemetry-complete
        let (outcome, error_category) = observability::classify_query_result(&result);
        self.metrics
            .record_query_request(QueryKind::Aggregated, outcome, error_category, seconds);
        let log_tenant = ctx.subject_tenant_id().to_string();
        log_operation_completed(
            OperationLogEntry::new(PdpOp::QueryAggregated, outcome, error_category)
                .tenant(Some(log_tenant.as_str()))
                .referenced_type(Some(log_referenced_type.as_str())),
        );
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-telemetry-complete
        result
    }

    /// The request's five stages, in the order Spec §6 fixes them. Split
    /// from the bucket cap and completion telemetry above so every exit
    /// below — `?` included — is capped, measured and logged exactly once,
    /// and so the `QueryInflightGuard` drops before the duration is read.
    ///
    /// The plugin is resolved **after** authorization, declaration
    /// resolution and the admissibility checks below. That position is
    /// fixed, not incidental: Spec §6 orders an unauthorized caller's
    /// rejection ahead of an unavailable plugin's, so the call must not be
    /// hoisted earlier by a future tidy-up.
    // @cpt-flow:cpt-cf-usage-collector-flow-query-aggregated-usage:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-query-aggregation:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-authorize-query:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-cross-tenant-read:p1
    //
    // Traceability (feature 2.6, usage-query): realizes
    // `flow-query-aggregated-usage` end to end. The gear's half of
    // `algo-period-end-selection` / `dod-period-end-selection` holds here
    // too: `time_range` stays a typed parameter dispatched identically to
    // `list_usage_records`'s (`inst-sel-uniform`), and `$filter` cannot
    // smuggle in a `window_start`/`window_end` predicate
    // (`query::RESERVED_FILTER_FIELDS`).
    // @cpt-flow:cpt-cf-usage-collector-flow-query-aggregated-usage:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-period-end-selection:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-period-end-selection:p1
    //
    // Traceability (feature 2.11, consistency-freshness-contract): one
    // dispatch to the bound plugin below, no retry, no session/pool
    // affinity, no replication-watermark wait — the aggregate half of
    // `algo-staleness-defensive-read` and of
    // `dod-consistency-no-stronger-read-claim`.
    // @cpt-dod:cpt-cf-usage-collector-dod-consistency-no-stronger-read-claim:p1
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    async fn dispatch_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError> {
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-scope
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-attribution
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-denied
        let scope = authz::authorize_list_usage_records(
            &self.enforcer,
            self.metrics.as_ref(),
            PdpOp::QueryAggregated,
            ctx,
        )
        .await
        .map_err(UsageCollectorError::from)?;
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-denied
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-attribution
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-scope

        // Authorization composed → track in-flight for the remainder of
        // this attempt (decremented on every exit below, `?` included).
        let _inflight =
            query::QueryInflightGuard::enter(self.metrics.as_ref(), QueryKind::Aggregated);

        // Resolve the queried meter's declaration before any plugin
        // dispatch: a meter declares exactly one fold, and this gear
        // serves that fold and no other — there is no request-supplied
        // aggregation to validate against a kind. An unresolvable type
        // (never declared, or an incomplete declaration) fails closed
        // here as a pre-dispatch 404, so the plugin stays pure
        // persistence and never sees a type it cannot resolve.
        // @cpt-flow:cpt-cf-usage-collector-flow-resolve-for-read:p1
        let declaration = self.type_resolver.resolve(&gts_type_id).await?;

        // The Spec §3.11 query-surface gate, all five checks — the rule
        // and why each exists: [`crate::domain::query`]. Runs after
        // resolving the declaration (there is nothing to check `group_by`
        // / `metadata_filter` against before then) and before composing
        // the PDP scope, recomputed per request.
        if let Some(filter) = query.filter() {
            reject_unpublished_filter_fields(filter)?;
            reject_off_label_literals(filter)?;
        }
        require_dimensions_declared(
            group_by,
            declaration.metadata_schema.declared_keys(),
            &gts_type_id,
        )?;
        require_metadata_filter_within_caps(metadata_filter)?;
        require_metadata_filter_keys_declared(
            metadata_filter,
            declaration.metadata_schema.declared_keys(),
            &gts_type_id,
        )?;

        // Resolved here and not earlier: Spec §6 fixes this as the first point
        // an unavailable plugin may be reported, after authorization,
        // declaration resolution and the admissibility checks above.
        let plugin = self
            .resolve_plugin_for(PluginOp::QueryAggregatedUsageRecords)
            .await?;

        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-scope
        let composed = compose_query_with_scope(query, &scope)?;
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-scope

        // The reference the plugin keys storage on, taken from the
        // declaration this call already resolved above — never derived
        // here (`MeterRef`'s own doc).
        let meter = MeterRef::new(declaration.type_uuid, gts_type_id.clone());

        // The fold is read from the resolved declaration and from nowhere
        // else — never a request parameter, never inferred from the type
        // identifier's shape — and is attached to the dispatched query so
        // the storage backend folds, not the gear. The gear's half of
        // `algo-query-fold-application` (the backend's own fold execution
        // and withdrawn-pair exclusion are the plugin's, per its SPI doc)
        // and of `dod-no-aggregation-parameter`: no public surface accepts
        // a caller-chosen fold.
        // @cpt-algo:cpt-cf-usage-collector-algo-query-fold-application:p1
        // @cpt-dod:cpt-cf-usage-collector-dod-no-aggregation-parameter:p1
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-dispatch
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-plugin-catch
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-plugin-catch-return
        observability::instrument_spi(
            self.metrics.as_ref(),
            PluginOp::QueryAggregatedUsageRecords,
            plugin.query_aggregated_usage_records(
                &meter,
                time_range,
                declaration.aggregation_fold,
                &composed,
                metadata_filter,
                group_by,
            ),
        )
        .await
        .map_err(|e| {
            lift_domain_error(
                DomainError::from(e),
                UnavailableRetryAfterSecs(self.unavailable_retry_after_secs),
                TargetNotConvergedRetryAfterSecs(self.target_not_converged_retry_after_secs),
            )
        })
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-plugin-catch-return
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-plugin-catch
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-dispatch
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "aggregate_query_tests.rs"]
mod aggregate_query_tests;
