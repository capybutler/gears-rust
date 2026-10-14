//! The gear's SPI-instrumentation and outcome-classification module: the
//! per-operation structured log entry
//! (`cpt-cf-usage-collector-algo-telemetry-log-correlation`,
//! `cpt-cf-usage-collector-dod-telemetry-log-correlation`), the SPI dispatch
//! wrapper every plugin call goes through, and the closed-vocabulary
//! classifiers that turn a completed attempt into the labels its metric and
//! its log line share.
//!
//! The log's vocabulary and its counter's cannot drift apart
//! (`inst-log-shared-vocabulary`): every caller passes the `as_str()` of the
//! very enum ([`crate::v2::domain::ports::metrics`]) it just gave its own
//! `UsageCollectorMetrics` call, never a hand-written literal.
//!
//! # No trace extraction in this gear
//!
//! This gear carries no `axum::Router` middleware of its own that opens a
//! request span or reads `traceparent`; on every path this repository can route
//! a request through today, the call already runs inside a span the API gateway
//! opened and already extracted `traceparent` onto, so a second extraction here
//! would re-read a header the edge already read.
//!
//! So [`log_operation_completed`] emits a bare `tracing::info!`, never inside a
//! span it opens itself, and sets **no** `trace_id` field of its own: the event
//! attaches to whatever span is current at the call site, and the correlation
//! holds structurally, by span nesting.
//!
//! # The absences (`inst-log-no-metadata`)
//!
//! [`OperationLogEntry`] has no field for a caller-supplied metadata value
//! and no field for invalidation reason text. That is what makes the
//! omission structural rather than a discipline a caller could forget —
//! there is no parameter to pass either through even by mistake.

use std::time::Duration;

use toolkit_macros::domain_model;
use toolkit_odata::CursorV1;
use tracing::Instrument;
use usage_collector_sdk::v2::{
    ConflictOutcome, FeedPage, FeedPosition, FeedStart, NotFoundReason, StoredUsageRecord,
    UsageCollectorError, UsageCollectorPluginError, ValidationReason,
};

use crate::v2::domain::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_domain_error,
};
use crate::v2::domain::ports::metrics::{
    FeedErrorCategory, IngestRequestErrorCategory, IngestRequestOutcome, PdpOp,
    PluginErrorCategory, PluginOp, QueryErrorCategory, RecordErrorCategory, RequestOutcome,
    UsageCollectorMetrics,
};

/// `tracing` target every [`log_operation_completed`] event carries, so a
/// test (or a log-pipeline filter) can select this family without matching
/// on the event's message text.
pub const LOG_TARGET: &str = "usage_collector::operation";

/// Implemented by this crate's closed `operation`-label enums
/// ([`PdpOp`], the only one used so far), so [`OperationLogEntry::new`] can
/// take the label enum directly instead of a bare `&'static str` — a
/// transposed call then fails to type-check instead of silently mislabeling
/// the log line.
pub trait OperationLabel {
    /// This label's `as_str()` value.
    fn operation_label(self) -> &'static str;
}

impl OperationLabel for PdpOp {
    fn operation_label(self) -> &'static str {
        self.as_str()
    }
}

/// Implemented by this crate's closed `outcome`-label enums. See
/// [`OperationLabel`].
pub trait OutcomeLabel {
    /// This label's `as_str()` value.
    fn outcome_label(self) -> &'static str;
}

impl OutcomeLabel for IngestRequestOutcome {
    fn outcome_label(self) -> &'static str {
        self.as_str()
    }
}

impl OutcomeLabel for RequestOutcome {
    fn outcome_label(self) -> &'static str {
        self.as_str()
    }
}

/// Implemented by this crate's closed `error_category`-label enums. See
/// [`OperationLabel`].
pub trait ErrorCategoryLabel {
    /// This label's `as_str()` value.
    fn error_category_label(self) -> &'static str;
}

impl ErrorCategoryLabel for IngestRequestErrorCategory {
    fn error_category_label(self) -> &'static str {
        self.as_str()
    }
}

impl ErrorCategoryLabel for QueryErrorCategory {
    fn error_category_label(self) -> &'static str {
        self.as_str()
    }
}

impl ErrorCategoryLabel for FeedErrorCategory {
    fn error_category_label(self) -> &'static str {
        self.as_str()
    }
}

/// One structured log entry, built from the same closed vocabularies
/// ([`crate::v2::domain::ports::metrics`]) the operation's own metric point
/// used.
///
/// `operation`, `outcome` and `error_category` are stored as `&'static str`,
/// but [`OperationLogEntry::new`] takes each as its own label-enum trait
/// ([`OperationLabel`], [`OutcomeLabel`], [`ErrorCategoryLabel`]) rather than
/// three bare strings, so a transposed call can't type-check. `tenant`,
/// `subject`, `resource` and `referenced_type` are opaque identifier strings
/// carried unchanged, present when the operation has exactly one value to
/// report and absent otherwise. Absent renders as no field at all, via
/// `tracing-core`'s blanket `Value` impl for `Option<T>`.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct OperationLogEntry<'a> {
    operation: &'static str,
    outcome: &'static str,
    error_category: &'static str,
    tenant: Option<&'a str>,
    subject: Option<&'a str>,
    resource: Option<&'a str>,
    referenced_type: Option<&'a str>,
}

impl<'a> OperationLogEntry<'a> {
    /// Start a new entry. `operation`, `outcome` and `error_category`
    /// **MUST** be the same label-enum values the call site just gave its
    /// own metrics call (`inst-log-shared-vocabulary`), which this
    /// constructor cannot enforce: the counters and this module share no
    /// supertype, only the convention that every caller is a `Service`
    /// operation boundary. The three trait bounds do enforce that the right
    /// *kind* of label landed in the right slot — a transposed call (e.g.
    /// an error-category label passed where outcome belongs) fails to
    /// type-check instead of silently mislabeling the log line.
    #[must_use]
    pub fn new(
        operation: impl OperationLabel,
        outcome: impl OutcomeLabel,
        error_category: impl ErrorCategoryLabel,
    ) -> Self {
        Self {
            operation: operation.operation_label(),
            outcome: outcome.outcome_label(),
            error_category: error_category.error_category_label(),
            tenant: None,
            subject: None,
            resource: None,
            referenced_type: None,
        }
    }

    /// Carry the attributed tenant, an opaque string
    /// (`inst-log-identifiers-here`).
    #[must_use]
    pub fn tenant(mut self, tenant: Option<&'a str>) -> Self {
        self.tenant = tenant;
        self
    }

    /// Carry the attributed subject, an opaque string. Absent where the
    /// operation's attribution tuple carries none, or where the operation is
    /// not scoped to one.
    #[must_use]
    pub fn subject(mut self, subject: Option<&'a str>) -> Self {
        self.subject = subject;
        self
    }

    /// Carry the attributed resource, an opaque string. Absent where the
    /// operation spans more than one resource.
    #[must_use]
    pub fn resource(mut self, resource: Option<&'a str>) -> Self {
        self.resource = resource;
        self
    }

    /// Carry the referenced GTS type, an opaque string. Absent where the
    /// operation spans more than one type.
    #[must_use]
    pub fn referenced_type(mut self, referenced_type: Option<&'a str>) -> Self {
        self.referenced_type = referenced_type;
        self
    }
}

/// Emit the one structured log entry a completed API operation owes
/// (`inst-log-return`), via a bare `tracing::info!` and never inside a span
/// of its own — see this module's doc for why.
///
/// Carries no caller-supplied metadata value and no invalidation reason text
/// (`inst-log-no-metadata`): [`OperationLogEntry`] has no field for either.
// @cpt-algo:cpt-cf-usage-collector-algo-telemetry-log-correlation:p2
// @cpt-dod:cpt-cf-usage-collector-dod-telemetry-log-correlation:p2
pub fn log_operation_completed(entry: OperationLogEntry<'_>) {
    tracing::info!(
        target: LOG_TARGET,
        operation = entry.operation,
        outcome = entry.outcome,
        error_category = entry.error_category,
        tenant = entry.tenant,
        subject = entry.subject,
        resource = entry.resource,
        referenced_type = entry.referenced_type,
    );
}

/// Default plugin-host SPI dispatch cutoff, used for every [`PluginOp`]
/// except [`PluginOp::QueryAggregatedUsageRecords`] (see
/// [`AGGREGATED_QUERY_SPI_DISPATCH_TIMEOUT`]).
pub(crate) const PLUGIN_SPI_DISPATCH_TIMEOUT: Duration = Duration::from_millis(250);

/// `QueryAggregatedUsageRecords`'s own SPI dispatch cutoff: the
/// aggregated-query backend call gets 425 ms of the 500 ms end-to-end query
/// budget, where the blanket [`PLUGIN_SPI_DISPATCH_TIMEOUT`] would kill a
/// plugin still inside its own documented allowance.
pub(crate) const AGGREGATED_QUERY_SPI_DISPATCH_TIMEOUT: Duration = Duration::from_millis(425);

/// The SPI dispatch cutoff [`instrument_spi`] applies for `op`. A `match`
/// with no wildcard arm, so a new [`PluginOp`] variant forces a decision
/// here instead of silently inheriting the default.
pub(crate) const fn spi_dispatch_timeout(op: PluginOp) -> Duration {
    match op {
        PluginOp::QueryAggregatedUsageRecords => AGGREGATED_QUERY_SPI_DISPATCH_TIMEOUT,
        PluginOp::CreateUsageRecords
        | PluginOp::ListUsageRecords
        | PluginOp::GetUsageRecord
        | PluginOp::ReadFeedPage
        | PluginOp::GetReconciliationMetadata => PLUGIN_SPI_DISPATCH_TIMEOUT,
    }
}

/// Classify a Plugin SPI error for `uc_plugin_accept_errors_total`.
///
/// Only backend-classified faults increment the counter:
/// [`UsageCollectorPluginError::Transient`] / `Internal` → `backend_error`.
/// The deterministic domain-typed variants (`UsageRecord*`,
/// `IdempotencyConflict`) are caller-visible outcomes, **not** plugin faults,
/// and MUST NOT increment it (their duration sample is still recorded): the
/// counter's `error_category` vocabulary has no value for them. Host-side
/// dispatch deadline expiry is recorded by `instrument_spi` as `timeout`,
/// before this classifier is consulted.
pub(crate) fn backend_error_category(
    err: &UsageCollectorPluginError,
) -> Option<PluginErrorCategory> {
    match err {
        UsageCollectorPluginError::Transient { .. } | UsageCollectorPluginError::Internal(_) => {
            Some(PluginErrorCategory::BackendError)
        }
        _ => None,
    }
}

/// Plugin-host SPI-dispatch instrumentation wrapper. Times a single Plugin SPI
/// call into `uc_plugin_call_duration_seconds{operation}` (success OR error —
/// an error completion is still a dispatch completion) and, on a
/// backend-classified fault, increments
/// `uc_plugin_accept_errors_total{operation, error_category}`. The SPI outcome
/// is returned unchanged; metric emission is fire-and-forget and never mutates
/// or reorders the result. A free function (not a `Service` method) so it is
/// reusable inside the concurrent fan-out closures of the batch path.
///
/// **The plugin-host span.** `fut` is `.instrument`ed with a
/// `plugin_spi_dispatch` span carrying `operation = op.as_str()`, opened
/// around the dispatch alone (not the metric emission above, which runs after
/// the span has closed). W3C trace context propagates on span **parentage**,
/// not attributes, so a span opened here inherits the caller's ambient request
/// span and end-to-end traces run gateway → core → plugin → backend with no
/// further wiring. It carries only `operation`: no identifier is in scope at
/// this seam, `instrument_spi` taking no identifier parameter at all.
// @cpt-algo:cpt-cf-usage-collector-algo-slo-latency-attribution:p1
// @cpt-dod:cpt-cf-usage-collector-dod-slo-latency-attribution-boundary:p1
// @cpt-dod:cpt-cf-usage-collector-dod-otlp-push-emission:p2
// @cpt-algo:cpt-cf-usage-collector-algo-plugin-dispatch:p1
pub(crate) async fn instrument_spi<T>(
    metrics: &dyn UsageCollectorMetrics,
    op: PluginOp,
    fut: impl std::future::Future<Output = Result<T, UsageCollectorPluginError>>,
) -> Result<T, UsageCollectorPluginError> {
    let span = tracing::info_span!("plugin_spi_dispatch", operation = op.as_str());
    let start = std::time::Instant::now();
    // Steps 6 and 7 (continue the host's trace span over the dispatch; try
    // the SPI method). Narrowed to this one statement so neither step's span
    // swallows the metric emission after it.
    // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-trace-span
    // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-invoke
    let (result, timed_out) =
        match tokio::time::timeout(spi_dispatch_timeout(op), fut.instrument(span)).await {
            Ok(result) => (result, false),
            Err(_) => (
                Err(UsageCollectorPluginError::Transient {
                    detail: format!("plugin SPI dispatch '{}' timed out", op.as_str()),
                    retry_after_seconds: None,
                }),
                true,
            ),
        };
    // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-invoke
    // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-trace-span
    let seconds = start.elapsed().as_secs_f64();
    match &result {
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-duration
        Ok(_) => metrics.record_plugin_call(op, seconds),
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-duration
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-catch
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-error-duration
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-error-counter
        Err(e) => {
            metrics.record_plugin_call(op, seconds);
            if timed_out {
                metrics.record_plugin_accept_error(op, PluginErrorCategory::Timeout);
            } else if let Some(category) = backend_error_category(e) {
                metrics.record_plugin_accept_error(op, category);
            }
        } // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-error-counter
          // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-error-duration
          // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-catch
    }
    // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-return
    result
    // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-return
}

/// Project a per-record ingestion rejection onto the closed
/// `uc_ingestion_records_total.error_category` vocabulary (keyed off the typed
/// `UsageCollectorError` variant + its discriminators, not a wire string).
pub(crate) fn classify_record_error(err: &UsageCollectorError) -> RecordErrorCategory {
    match err {
        UsageCollectorError::PermissionDenied { .. } => RecordErrorCategory::Authz,
        // The target rule — a derived identifier resolving to nothing — is a
        // `NotFound`, separated from an ordinary missing entry by the typed
        // `NotFoundReason` rather than by matching caller-facing prose.
        // `UsageRecordNotFound` shares the wildcard's body but keeps an
        // explicit arm: a decided classification, not an unconsidered case.
        #[allow(clippy::match_same_arms)]
        UsageCollectorError::NotFound { reason, .. } => match reason {
            NotFoundReason::DeclarationNotFound => RecordErrorCategory::UnknownUsageType,
            NotFoundReason::InvalidationTargetNotFound => RecordErrorCategory::InvalidationRule,
            NotFoundReason::UsageRecordNotFound => RecordErrorCategory::SemanticsViolation,
            // Required: `NotFoundReason` is `#[non_exhaustive]` and foreign.
            // A future lookup kind falls here silently, so a new variant needs
            // an explicit arm above.
            _ => RecordErrorCategory::SemanticsViolation,
        },
        UsageCollectorError::InvalidArgument { reason, .. } => match reason {
            // Both, and only these two: DESIGN §3.11.5 defines the category as
            // "the metadata size-cap or closed-shape rejection", which is
            // exactly `MetadataValidation` (the cap, from
            // `metadata_size_exceeded`) and `UnknownMetadataKey` (the closed
            // shape). Lumping them is the spec, not a shortcut.
            //
            // A declared key whose VALUE breaks a declared constraint is
            // neither, carries `Validation`, and falls to the
            // `semantics_violation` arm below — where §3.11.5 puts "every
            // other meter-declaration or ingest-shape rejection".
            ValidationReason::UnknownMetadataKey | ValidationReason::MetadataValidation => {
                RecordErrorCategory::MetadataSize
            }
            // The copy rule, the one invalidation rule that arrives here as an
            // `InvalidArgument`; target resolution joins it from the `NotFound`
            // arm above, and the two are all this category covers.
            ValidationReason::InvalidationFieldMismatch => RecordErrorCategory::InvalidationRule,
            _ => RecordErrorCategory::SemanticsViolation,
        },
        UsageCollectorError::Conflict { outcome, .. } => match outcome {
            // A second invalidation rejected as already invalidated is the
            // dedup conflict, not a target-resolution or copy rule; a target
            // not yet converged is the invalidation rule.
            ConflictOutcome::IdempotencyConflict | ConflictOutcome::AlreadyInvalidated { .. } => {
                RecordErrorCategory::IdempotencyConflict
            }
            ConflictOutcome::TargetNotConverged { .. } => RecordErrorCategory::InvalidationRule,
            _ => RecordErrorCategory::SemanticsViolation,
        },
        _ => RecordErrorCategory::PluginError,
    }
}

/// Project a completed query attempt onto `(outcome, error_category)` for
/// `uc_query_requests_total`.
///
/// **Seam note:** `cursor_decode`, `order_mismatch` and
/// `missing_security_context` surface only at the REST boundary, upstream of
/// this service seam, so they stay reserved-not-emitted here.
/// `filter_mismatch` is **not** one of them: the query a continuation is
/// bound to is compared behind the service, by
/// [`crate::v2::domain::list_query::require_cursor_fingerprint`], so this seam is
/// the only place that category can arise.
///
/// A PDP-transport failure and a plugin fault both surface as
/// `ServiceUnavailable` at this seam and both map to `plugin_error`; the
/// authoritative PDP-unavailability signal is the foundation-owned
/// `uc_pdp_failures_total`.
///
/// `ValidationReason::CursorBeyondRetention` deliberately gets no arm here: the
/// replay-refusal signal lives on
/// `uc_feed_requests_total{error_category="cursor_beyond_retention"}`, and no
/// query path raises it in any case.
pub(crate) fn classify_query_result<T>(
    result: &Result<T, UsageCollectorError>,
) -> (RequestOutcome, QueryErrorCategory) {
    match result {
        Ok(_) => (RequestOutcome::Success, QueryErrorCategory::None),
        Err(UsageCollectorError::PermissionDenied { .. }) => {
            (RequestOutcome::Denied, QueryErrorCategory::Authz)
        }
        // Discriminated on the typed `reason`, as `classify_record_error` does
        // on the ingestion side: the three scoped queries resolve the queried
        // meter before dispatch and so raise only `DeclarationNotFound`, while
        // `get_usage_record` resolves no declaration and raises
        // `UsageRecordNotFound`.
        //
        // **This arm is only safe to read as a genuine miss because the point
        // path routes its PDP denies away before reaching here**:
        // `collapse_deny_to_not_found` makes a deny byte-identical to a miss,
        // and nothing here can read that back out. `get_usage_record`
        // classifies the decision before the collapse runs. A second path that
        // collapses a deny into this reason without doing the same would
        // relabel denies `record_not_found`.
        #[allow(clippy::match_same_arms)]
        Err(UsageCollectorError::NotFound { reason, .. }) => match reason {
            NotFoundReason::UsageRecordNotFound => {
                (RequestOutcome::Error, QueryErrorCategory::RecordNotFound)
            }
            NotFoundReason::DeclarationNotFound => {
                (RequestOutcome::Error, QueryErrorCategory::UnknownUsageType)
            }
            // Unreachable from any query path — an invalidation's target is
            // resolved on the ingestion side. Given an explicit arm anyway:
            // a decided classification, which the wildcard cannot record.
            NotFoundReason::InvalidationTargetNotFound => {
                (RequestOutcome::Error, QueryErrorCategory::UnknownUsageType)
            }
            // Required: `NotFoundReason` is `#[non_exhaustive]` and foreign.
            // A future lookup kind falls here silently, so a new variant needs
            // an explicit arm above.
            _ => (RequestOutcome::Error, QueryErrorCategory::UnknownUsageType),
        },
        // A cursor rejection is a continuation refused, and the two conditions
        // behind it do not share a label. The discriminator is the upstream
        // `toolkit_odata` error the variant carries, the same value that
        // decides the wire code.
        Err(UsageCollectorError::CursorRejected { source, .. }) => match source {
            toolkit_odata::Error::FilterMismatch => {
                (RequestOutcome::Error, QueryErrorCategory::FilterMismatch)
            }
            // Every other cursor rejection is an `INVALID_CURSOR`, folded into
            // `query_budget` — a known imprecision on this seam, left as it was
            // so the vocabulary pass does not move an operator's series.
            _ => (RequestOutcome::Error, QueryErrorCategory::QueryBudget),
        },
        // Everything else reaching here is a query-surface rejection — a
        // `$filter` naming a reserved field, an undeclared `group_by` /
        // `metadata_filter` key, an over-cap aggregate result, or an
        // `$orderby` that cannot be floored into a keyset. The mandatory range
        // cannot land here: it is validated at the edge, where the typed
        // parameter is parsed, before the service is entered.
        Err(UsageCollectorError::InvalidArgument { .. }) => {
            (RequestOutcome::Error, QueryErrorCategory::QueryBudget)
        }
        Err(_) => (RequestOutcome::Error, QueryErrorCategory::PluginError),
    }
}

/// Project a completed feed page attempt onto `(outcome, error_category)`
/// for `uc_feed_requests_total`.
///
/// **A function of its own, never an arm on [`classify_query_result`].** The
/// two vocabularies overlap on four spellings and disagree on the other two
/// this counter carries: `cursor_beyond_retention`, which the query counter has
/// no value for, and `argument_rejected` rather than `query_budget`. Folding
/// the feed into the query classifier would land the retention refusal on
/// `query_budget`, silencing the alert that watches the retention rate.
///
/// The retention refusal arrives as an `InvalidArgument`: the plugin raises
/// [`UsageCollectorPluginError::CursorBeyondRetention`], `DomainError` keeps it
/// whole, and `domain/error.rs`'s lift turns it into a `400` on `cursor`
/// carrying [`ValidationReason::CursorBeyondRetention`]. The typed reason
/// discriminates it here — not a substring of the caller-facing `detail`.
///
/// **A PDP outage is counted as [`FeedErrorCategory::PluginError`]:** a
/// transport failure and a plugin fault both surface as `ServiceUnavailable` at
/// this seam, and `Authz` is reserved for a *completed* decision. Same fold and
/// same remedy as [`classify_query_result`].
pub(crate) fn classify_feed_result<T>(
    result: &Result<T, UsageCollectorError>,
) -> (RequestOutcome, FeedErrorCategory) {
    match result {
        Ok(_) => (RequestOutcome::Success, FeedErrorCategory::None),
        Err(UsageCollectorError::PermissionDenied { .. }) => {
            (RequestOutcome::Denied, FeedErrorCategory::Authz)
        }
        // Every guard on the feed's decode path raises this one variant, and
        // all of them mean the same thing to an operator: a continuation the
        // gateway would not accept. Which guard fired is a `detail`, not a
        // label — `CursorDecode` is the one value for the lot.
        Err(UsageCollectorError::CursorRejected { .. }) => {
            (RequestOutcome::Error, FeedErrorCategory::CursorDecode)
        }
        Err(UsageCollectorError::InvalidArgument { reason, .. })
            if *reason == ValidationReason::CursorBeyondRetention =>
        {
            (
                RequestOutcome::Error,
                FeedErrorCategory::CursorBeyondRetention,
            )
        }
        // The behind-the-service `limit` / subscription-breadth check: a
        // caller-surface argument rejection, not a plugin fault. Matched on the
        // typed reason rather than the bare variant so a future reason added to
        // those call sites does not fold in here silently.
        Err(UsageCollectorError::InvalidArgument { reason, .. })
            if *reason == ValidationReason::Validation =>
        {
            (RequestOutcome::Error, FeedErrorCategory::ArgumentRejected)
        }
        // A catch-all is required here: both `InvalidArgument` arms above are
        // guarded, and a match guard never contributes to exhaustiveness.
        // `ValidationReason` and `UsageCollectorError` are also
        // `#[non_exhaustive]`, so a reason this seam has never seen falls here
        // and counts as a `plugin_error` silently; a new one reachable from this
        // seam needs an explicit arm above.
        Err(_) => (RequestOutcome::Error, FeedErrorCategory::PluginError),
    }
}

/// Reports, without failing the request, a live feed read the plugin
/// answered with no continuation.
///
/// The disposition sits on the *plugin* — a next cursor with every page of a
/// live read, short pages included — and no compiler holds a plugin to it.
/// `FeedPage.next` is an `Option<FeedPosition>` on a conforming and a breaching
/// implementation alike, so a plugin that stops minting one recompiles clean
/// and serves a `200`.
///
/// Diagnosis only, and deliberately so. The gateway must not *decide* `next`
/// — a gateway-side branch would be a second, disagreeing authority — so the
/// only dispositions here are "serve and report" and "refuse". Refusing would
/// deny a charging consumer a page of entries that are themselves correct and
/// final; serving turns the breach into an `error!` at the point of fault,
/// one request before a consumer misreads the absent cursor as the end of a
/// stream that has not ended.
///
/// A bounded replay (`until` supplied) is exempt: `None` there is the
/// ordinary last page.
pub(crate) fn report_absent_live_continuation(
    page: &FeedPage<FeedPosition, StoredUsageRecord>,
    until_supplied: bool,
) {
    if until_supplied || page.next.is_some() {
        return;
    }
    tracing::error!(
        entries = page.entries.len(),
        "usage-collector storage plugin served a live feed page with no next position; \
         DESIGN section 3.2 requires one on every page of a live read, short pages \
         included, and the consumer will read its absence as the end of the stream"
    );
}

/// Lift a `read_feed_page` failure, knowing where the read was asked to
/// begin.
///
/// One pair is not the error it lifts to under the frozen `From`: a
/// `CursorBeyondRetention` answered to a [`FeedStart::Oldest`] request. There
/// is no cursor parameter on such a request, so the context-free lift
/// (`domain/error.rs`) would raise a `400` field violation naming `cursor` — a
/// parameter the caller never sent — advising them to restart the subscription
/// from its oldest retained position, which is exactly what they did.
///
/// So it is lifted as the host-contract breach it is, which also makes
/// `classify_feed_result` count it under `plugin_error` instead of
/// `cursor_beyond_retention`: that category backs the single feed alert, so a
/// misbehaving plugin firing it would page an operator about a consumer that is
/// not behind at all.
///
/// Every other error takes the context-free `From`, unchanged.
// @cpt-dod:cpt-cf-usage-collector-dod-retention-feed-conformance:p2
pub(crate) fn lift_feed_dispatch_error(
    err: UsageCollectorPluginError,
    start: &FeedStart<&CursorV1>,
    unavailable_retry_after_secs: u64,
    target_not_converged_retry_after_secs: u64,
) -> UsageCollectorError {
    match (start, &err) {
        (FeedStart::Oldest, UsageCollectorPluginError::CursorBeyondRetention) => {
            UsageCollectorError::internal(
                "usage-collector storage plugin answered CURSOR_BEYOND_RETENTION to a feed read \
                 that supplied no cursor; the retention refusal reads a caller-supplied cursor \
                 only, a FeedStart::Oldest request beginning by construction at the oldest \
                 position the subscription still serves (DESIGN section 3.2)",
            )
        }
        _ => lift_domain_error(
            DomainError::from(err),
            UnavailableRetryAfterSecs(unavailable_retry_after_secs),
            TargetNotConvergedRetryAfterSecs(target_not_converged_retry_after_secs),
        ),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "observability_tests.rs"]
mod observability_tests;
