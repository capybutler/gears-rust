//! The reconciliation read's pure half — granularity admission, scope
//! construction, and the guard that holds a plugin's answer to the
//! declaration's fold — and its orchestration.
//!
//! The public entry point is
//! [`Service::get_reconciliation_metadata`], whose `impl Service` block lives
//! at the foot of this module rather than in `domain/service.rs`: the
//! orchestration belongs with the granularity admission, scope construction
//! and fold guard it is assembled from. Those three need no plugin, PDP or
//! registry, which is what makes them unit-testable without any of them. It
//! mirrors `domain::feed`'s split for the Feed Gateway.
//!
//! **Two things called "scope" meet on this path and they are unrelated.**
//! The wire parameter `scope` is the reporting *granularity*
//! ([`usage_collector_sdk::ReconciliationScope`]); the gear's `scope` /
//! `scope_expr` / `&ast::Expr` vocabulary is the *compiled PDP scope*. This
//! module names the first `granularity` throughout and never binds it to a
//! variable called `scope`, so a reader of either call site can tell which is
//! which. The DTO field keeps the wire spelling, because
//! `usage-collector-v1.yaml` fixes it.

use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    AggregationFold, MeterRef, MeterTypeId, QuantitySummary, ReconciliationMetadata,
    ReconciliationScope, TimeRange, USAGE_RECORD_RESOURCE, UsageCollectorError, ValidationReason,
};
use uuid::Uuid;

use crate::domain::authz;
use crate::domain::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_domain_error,
};
use crate::domain::observability::{self, OperationLogEntry, log_operation_completed};
use crate::domain::ports::metrics::{PdpOp, PluginOp, QueryKind};
use crate::domain::service::Service;

/// The one granularity v1 serves, spelled as `usage-collector-v1.yaml`'s
/// `scope` enum spells it.
pub const GRANULARITY_TENANT_GTS_TYPE: &str = "tenant_gts_type";

/// The granularities that are reserved and not served.
///
/// Parsed rather than left to fall through to the unknown arm, because
/// `cpt-cf-usage-collector-dod-reconciliation-caller-scopes-reserved` requires
/// a rejection that **states they are not served**: they are blocked on a
/// platform identity plane that carries neither gear name nor tenant, and a
/// caller told "unknown" will believe they misspelled something. Widening the
/// enum to serve them later is additive.
///
/// `reconciliation_tests`'s reserved-granularity test iterates this
/// constant rather than hardcoding its members, so a third reserved value
/// is covered automatically.
pub const RESERVED_GRANULARITIES: [&str; 2] = ["caller", "caller_tenant"];

/// Build an `InvalidArgument` naming `field`, with a fixed `Validation`
/// reason and this crate's own resource type.
///
/// Mirrors `domain::feed::invalid_argument`. No public SDK constructor fits a
/// caller-chosen `field`/`detail` pair here: `UsageCollectorError` has no
/// `invalid_argument_on` constructor, and the closest shape,
/// `newtype_validation`, is a private `fn` in the SDK crate
/// (`usage-collector-sdk/src/error.rs`) — not reachable from this crate. So
/// this constructs the variant directly, the documented fallback for exactly
/// this situation.
fn invalid_argument(field: &str, detail: impl Into<String>) -> UsageCollectorError {
    UsageCollectorError::InvalidArgument {
        resource_type: USAGE_RECORD_RESOURCE.to_owned(),
        resource_name: None,
        field: field.to_owned(),
        reason: ValidationReason::Validation,
        detail: detail.into(),
    }
}

/// Admit a granularity, or reject it naming what is wrong with it.
///
/// Returns `()` rather than a parsed value: v1 admits exactly one
/// granularity, so a successfully parsed one carries no information a caller
/// does not already have. A second served granularity turns this into a real
/// parse, and the enum it returns is where the branch would go.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] with a `scope` field violation,
/// in two distinguishable flavours: reserved-and-not-served, and unknown.
/// Both name the offending value.
// @cpt-flow:cpt-cf-usage-collector-flow-reconciliation-reserved-scope:p3
// @cpt-dod:cpt-cf-usage-collector-dod-reconciliation-caller-scopes-reserved:p3
// @cpt-algo:cpt-cf-usage-collector-algo-reconciliation-request-admission:p2
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub fn parse_granularity(raw: &str) -> Result<(), UsageCollectorError> {
    if raw == GRANULARITY_TENANT_GTS_TYPE {
        return Ok(());
    }
    if RESERVED_GRANULARITIES.contains(&raw) {
        return Err(invalid_argument(
            "scope",
            format!(
                "the `{raw}` reconciliation granularity is reserved and not served in v1, \
                 because the tenant plane carries no calling-gear identity; use \
                 `{GRANULARITY_TENANT_GTS_TYPE}`"
            ),
        ));
    }
    Err(invalid_argument(
        "scope",
        format!(
            "`{raw}` is not a reconciliation granularity; v1 serves \
             `{GRANULARITY_TENANT_GTS_TYPE}`"
        ),
    ))
}

/// The reporting scope a request names.
#[must_use]
pub fn build_scope(tenant_id: Uuid, gts_type_id: MeterTypeId) -> ReconciliationScope {
    ReconciliationScope {
        tenant_id,
        gts_type_id,
    }
}

/// Hold a plugin's summary to the branch the declared fold selects.
///
/// The gear resolves the declaration and passes the fold down; the plugin
/// picks the branch. A `SUM` meter answered with an observation count, or a
/// gauge answered with an accrued sum, is a **host-contract breach** — and
/// without this guard it would serialize as a perfectly valid-looking body,
/// on the surface an operator uses to decide whether an invoice is right.
///
/// Building the DTO from the *fold* instead would silently discard the
/// plugin's answer; building it from the variant without this check would
/// ship the wrong branch. This is the only place a wrong figure could
/// otherwise enter the path unnoticed.
///
/// This does not contradict the SPI's "Do not re-validate" obligation, which
/// binds a plugin not re-validating the gear. The direction here is the
/// reverse.
///
/// # Errors
///
/// [`UsageCollectorError::Internal`] naming both the fold that was declared
/// and the branch that came back. Not a caller error: nothing the caller
/// supplied could cause it.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub fn check_summary_branch(
    fold: AggregationFold,
    summary: &QuantitySummary,
) -> Result<(), UsageCollectorError> {
    let returned_accrues = matches!(summary, QuantitySummary::Accrued(_));
    if returned_accrues == QuantitySummary::accrues(fold) {
        return Ok(());
    }
    let branch = match summary {
        QuantitySummary::Accrued(_) => "Accrued",
        QuantitySummary::Observations(_) => "Observations",
    };
    Err(UsageCollectorError::internal(format!(
        "the storage plugin returned a `{branch}` reconciliation summary for a meter whose \
         declared fold is {fold:?}; the fold selects the branch and the plugin is given it"
    )))
}

impl Service {
    /// Per-scope reconciliation metadata — the Query Gateway's fourth read
    /// path (DESIGN §3.2).
    ///
    /// One `(tenant, GTS type)` scope per call, no filter, no grouping, no
    /// ordering and no paging: the scope parameters and the range are the
    /// whole request. The response is one typed body rather than a page —
    /// `cpt-cf-usage-collector-dod-canonical-page-envelope` binds the three
    /// list-shaped read paths and deliberately does not reach this one.
    ///
    /// The granularity is admitted at the REST edge and is not a parameter
    /// here: v1 serves one, so an admitted request carries nothing past the
    /// admission.
    ///
    /// **This path evaluates nothing it returns.** No threshold, no cadence
    /// read, no stalled-emitter signal
    /// (`cpt-cf-usage-collector-dod-reconciliation-no-stall-verdict`).
    /// Comparing a watermark against an expected cadence is the consumer's
    /// work.
    ///
    /// # Errors
    ///
    /// A PDP denial or an empty compiled scope, with nothing dispatched; a
    /// GTS type that does not resolve, before any dispatch; `ServiceUnavailable`
    /// where no plugin is bound; the plugin's own error; and
    /// [`UsageCollectorError::Internal`] where the plugin's summary branch
    /// disagrees with the declared fold ([`check_summary_branch`]).
    // @cpt-flow:cpt-cf-usage-collector-flow-reconciliation-compare-totals:p2
    // @cpt-flow:cpt-cf-usage-collector-flow-reconciliation-stalled-emitter:p2
    // @cpt-algo:cpt-cf-usage-collector-algo-reconciliation-request-admission:p2
    // @cpt-algo:cpt-cf-usage-collector-algo-reconciliation-figure-assembly:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-reconciliation-operator-surface:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-reconciliation-accepted-count:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-reconciliation-quantity-summary:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-reconciliation-watermarks:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-reconciliation-no-stall-verdict:p2
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub async fn get_reconciliation_metadata(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
    ) -> Result<ReconciliationMetadata, UsageCollectorError> {
        let started = std::time::Instant::now();
        let log_referenced_type = gts_type_id.as_str().to_owned();
        let result = self
            .dispatch_reconciliation_metadata(ctx, tenant_id, gts_type_id, time_range)
            .await;

        let seconds = started.elapsed().as_secs_f64();
        let (outcome, error_category) = observability::classify_query_result(&result);
        self.metrics.record_query_request(
            QueryKind::Reconciliation,
            outcome,
            error_category,
            seconds,
        );
        let log_tenant = tenant_id.to_string();
        log_operation_completed(
            OperationLogEntry::new(PdpOp::Reconciliation, outcome, error_category)
                .tenant(Some(log_tenant.as_str()))
                .referenced_type(Some(log_referenced_type.as_str())),
        );
        result
    }

    /// The request's stages, in the order per-record error precedence fixes
    /// them. Split from the completion telemetry above so every exit below —
    /// `?` included — is measured and logged exactly once.
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    async fn dispatch_reconciliation_metadata(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
    ) -> Result<ReconciliationMetadata, UsageCollectorError> {
        // Authorization first, so a caller with no permission does not
        // learn whether the meter exists.
        let scope = authz::authorize_get_reconciliation_metadata(
            &self.enforcer,
            self.metrics.as_ref(),
            PdpOp::Reconciliation,
            ctx,
        )
        .await
        .map_err(UsageCollectorError::from)?;

        // No `QueryInflightGuard` here, and no result-row or duration
        // observation below: DESIGN §3.11.5 admits `reconciliation` on
        // `uc_query_requests_total` alone. See `QueryKind::Reconciliation`.

        // The declared fold selects the summary's branch and reaches the
        // plugin as a parameter (`inst-assemble-fold`). Resolving here
        // also satisfies `inst-radmit-unresolved`: an unresolvable meter
        // fails closed before any dispatch.
        let declaration = self.type_resolver.resolve(&gts_type_id).await?;
        let fold = declaration.aggregation_fold;

        // Resolved here and not earlier: Spec §6 fixes this as the first point
        // an unavailable plugin may be reported, after authorization and
        // declaration resolution above.
        let plugin = self
            .resolve_plugin_for(PluginOp::GetReconciliationMetadata)
            .await?;

        // The reference the plugin keys storage on, taken from the
        // declaration this call already resolved above — never derived
        // here (`MeterRef`'s own doc).
        let meter = MeterRef::new(declaration.type_uuid, gts_type_id.clone());

        let metadata = observability::instrument_spi(
            self.metrics.as_ref(),
            PluginOp::GetReconciliationMetadata,
            plugin.get_reconciliation_metadata(tenant_id, &meter, time_range, fold, &scope),
        )
        .await
        .map_err(|e| {
            lift_domain_error(
                DomainError::from(e),
                UnavailableRetryAfterSecs(self.unavailable_retry_after_secs),
                TargetNotConvergedRetryAfterSecs(self.target_not_converged_retry_after_secs),
            )
        })?;

        check_summary_branch(fold, &metadata.quantity_summary)?;

        Ok(metadata)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "reconciliation_tests.rs"]
mod reconciliation_tests;
