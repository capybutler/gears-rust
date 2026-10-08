//! The gear's SPI-instrumentation and outcome-classification module: the
//! per-operation structured log entry
//! (`cpt-cf-usage-collector-algo-telemetry-log-correlation`,
//! `cpt-cf-usage-collector-dod-telemetry-log-correlation`), the SPI dispatch
//! wrapper every plugin call goes through, and the closed-vocabulary
//! classifiers that turn a completed attempt into the labels its metric and
//! its log line share.
//!
//! **Structured log entry**
//!
//! * [`LOG_TARGET`] — the `tracing` target [`log_operation_completed`]'s
//!   event carries, so a test or log-pipeline filter can select this family
//!   without matching on the event's message text.
//! * [`OperationLogEntry`] — the borrowed payload [`log_operation_completed`]
//!   takes, assembled via its label-enum-typed `new` and per-field setters so
//!   a transposed call fails to type-check instead of mislabeling the log
//!   line.
//! * [`log_operation_completed`] — the sole production callsite of the
//!   `tracing::info!` this module emits, called from every `Service`
//!   operation boundary at the same point each operation's own metric is
//!   recorded. The log's vocabulary and the counter's cannot drift apart
//!   (`inst-log-shared-vocabulary`): every caller passes the `as_str()` of
//!   the very enum ([`crate::domain::ports::metrics`]) it just gave its own
//!   `UsageCollectorMetrics` call, never a hand-written literal. See "No
//!   extraction layer in this gear" and "The absences" below for what it
//!   deliberately does not carry.
//!
//! **SPI dispatch instrumentation**
//!
//! * `instrument_spi` — wraps one Plugin SPI call: times it into
//!   `uc_plugin_call_duration_seconds{operation}`, enforces its dispatch
//!   deadline, and on a backend-classified fault increments
//!   `uc_plugin_accept_errors_total{operation, error_category}`. A free
//!   function, not a `Service` method, so it is reusable inside the batch
//!   path's concurrent fan-out closures.
//! * `spi_dispatch_timeout` — the per-[`PluginOp`] cutoff `instrument_spi`
//!   applies, backed by `PLUGIN_SPI_DISPATCH_TIMEOUT` and
//!   `AGGREGATED_QUERY_SPI_DISPATCH_TIMEOUT` (the aggregated-query path's
//!   own, wider allowance against its larger end-to-end budget).
//! * `backend_error_category` — classifies a Plugin SPI error for
//!   `uc_plugin_accept_errors_total`; only backend-classified faults
//!   increment it, never a deterministic domain-typed outcome.
//!
//! **Outcome classification**
//!
//! * `classify_record_error` — projects a per-record ingestion rejection
//!   onto the closed `uc_ingestion_records_total.error_category` vocabulary.
//! * `classify_query_result` — projects a completed query attempt onto
//!   `(outcome, error_category)` for `uc_query_requests_total`.
//! * `classify_feed_result` — the feed's own version of the same
//!   projection: a function of its own rather than an arm on
//!   `classify_query_result`, because the two vocabularies overlap on four
//!   spellings and disagree on two others.
//!
//! **Feed-page diagnostics**
//!
//! * `report_absent_live_continuation` — logs, without failing the
//!   request, a live feed read the plugin answered with no continuation
//!   (DESIGN §3.2).
//! * `lift_feed_dispatch_error` — lifts a `read_feed_page` failure,
//!   correcting the one pair the context-free lift gets wrong: a
//!   `CursorBeyondRetention` answered to a [`FeedStart::Oldest`] request.
//!
//! # No extraction layer in this gear
//!
//! `cpt-cf-usage-collector-algo-telemetry-log-correlation`'s step 7
//! (`inst-log-trace-context`) says to "attach the entry to the ambient trace
//! context propagated with the request". This gear carries **no**
//! `axum::Router` middleware of its own that opens a request span or reads
//! `traceparent` — measured: no hits for `TraceLayer`, `make_span_with`,
//! `info_span!` or `debug_span!` under `api/`, and
//! [`crate::api::rest::routes::register_routes`] layers only
//! `axum::Extension<Arc<Service>>`.
//!
//! That is not a gap. `TraceLayer` appears in one gear repo-wide —
//! `gears/system/api-gateway/src/gear.rs`, whose `rest_finalize` merges every
//! embedded gear's routes onto **one shared `axum::Router`** before
//! `apply_middleware_stack` wraps the whole thing — so this gear's routes run
//! *inside* the gateway's `http_request` span whenever they are reached
//! in-process. The gateway's reverse-proxy fallback to an out-of-process gear
//! pod still traverses that middleware stack first: it is "Mounted BEFORE the
//! middleware stack so proxied requests traverse the same auth / tracing /
//! error-mapping layers as native routes". `docs/DESIGN.md`
//! `cpt-cf-usage-collector-topology-gear-runtime` states it normatively: "The
//! gear runs behind the platform API gateway."
//!
//! **This holds as the gear is deployed and configured, not as something the
//! Rust code enforces.** A third serving path exists in the framework and
//! reaches neither of the two above: `toolkit`'s `compose_oop_router` builds a
//! fresh, host-less `Router::new()` from every `RestApiCap` gear, and
//! `oop_serve.rs` binds it under its own middleware stack, where a sweep for
//! `TraceLayer`, `info_span!`, `otel::`, `set_parent_from_headers` and
//! `traceparent` returns nothing — a request landing there gets no span and no
//! extraction. That path is gated on `RuntimeKind::Oop` plus an
//! `execution.executable_path`, and this gear declares only `[lib]`, so it is
//! unreachable today. A future usage-collector binary plus an `OoP` config
//! would reach it and silently drop the correlation identifier.
//!
//! **On the two paths this repository can route a request through today, every
//! inbound call already runs inside a span the gateway opened and already
//! extracted `traceparent` onto** (`api-gateway/src/gear.rs`, the repo's one
//! production caller of `toolkit_http::otel::set_parent_from_headers`), so a
//! second extraction here would re-read a header the edge already read.
//!
//! So [`log_operation_completed`] does the minimum that is NOT redundant: a
//! bare `tracing::info!`, never inside a span it opens itself, so the event
//! attaches to whatever span is current at the call site — in production the
//! gateway's `http_request` span, already carrying `trace_id` /
//! `parent.trace_id`. This module sets **no** `trace_id` field of its own:
//! the correlation holds structurally, by span nesting, which is
//! `inst-log-trace-context`'s own wording ("attach the entry to the ambient
//! trace context", not "copy the trace id onto the entry").
//!
//! `observability_tests.rs` stands up its own span to have anything to
//! observe, exactly because this gear has none of its own to reuse.
//!
//! # The absences (`inst-log-no-metadata`)
//!
//! [`OperationLogEntry`] has no field for a caller-supplied metadata value
//! and no field for invalidation reason text. That is what makes the
//! omission structural rather than a discipline a caller could forget —
//! there is no parameter to pass either through even by mistake.
//!
//! A field declared `tracing::field::Empty` at span creation and never
//! recorded is a silent no-op rather than an error, which is why
//! `observability_tests.rs`'s absent-`traceparent` test
//! (`inst-log-take-correlation`'s "rather than generating one") is meaningful
//! only paired with its sibling driving a present `traceparent` through the
//! same span: an always-undeclared field would pass the absence assertion for
//! the wrong reason.

use std::time::Duration;

use toolkit_odata::CursorV1;
use tracing::Instrument;
use usage_collector_sdk::{
    ConflictOutcome, FeedPage, FeedPosition, FeedStart, NotFoundReason, StoredUsageRecord,
    UsageCollectorError, UsageCollectorPluginError, ValidationReason,
};

use crate::domain::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_domain_error,
};
use crate::domain::ports::metrics::{
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

/// One structured log entry (`cpt-cf-usage-collector-dod-telemetry-log-correlation`),
/// built from the same closed vocabularies
/// ([`crate::domain::ports::metrics`]) the operation's own metric point
/// used.
///
/// `operation`, `outcome` and `error_category` are stored as `&'static str`,
/// but [`OperationLogEntry::new`] takes each as its own label-enum trait
/// ([`OperationLabel`], [`OutcomeLabel`], [`ErrorCategoryLabel`]) rather than
/// three bare strings, so a transposed call can't type-check. `tenant`,
/// `subject`, `resource` and
/// `referenced_type` are the diagnostic identifiers
/// `cpt-cf-usage-collector-algo-opaque-identifier-handling` governs: opaque
/// strings carried unchanged, present when the operation has exactly one
/// value to report and absent otherwise. Absent renders as no field at all,
/// via `tracing-core`'s blanket `Value` impl for `Option<T>`, which is what
/// keeps absence and presence distinguishable to a test.
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
    /// operation's attribution tuple carries none (subject is optional on
    /// ingestion, `cpt-cf-usage-collector-fr-subject-attribution`) or where
    /// the operation is not scoped to one.
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

/// `QueryAggregatedUsageRecords`'s own SPI dispatch cutoff. DESIGN
/// §3.11.2's default planning value gives the aggregated-query backend call
/// up to 425 ms of the 500 ms end-to-end `nfr-query-latency` budget; the
/// blanket [`PLUGIN_SPI_DISPATCH_TIMEOUT`] would kill a plugin still inside
/// its own documented allowance.
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
/// counter's `error_category` vocabulary in DESIGN §3.11.5 has no value for
/// them. Host-side dispatch deadline expiry is recorded by `instrument_spi`
/// as `timeout`, before this classifier is consulted.
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

/// Plugin-host SPI-dispatch instrumentation wrapper. Realizes two of
/// `cpt-cf-usage-collector-algo-plugin-dispatch`'s steps directly — continuing
/// the host's trace span over the backend dispatch, and trying the SPI
/// method — and additionally times a single Plugin SPI call into
/// `uc_plugin_call_duration_seconds{operation}`
/// (success OR error — an error completion is still a dispatch completion) and,
/// on a backend-classified fault, increment
/// `uc_plugin_accept_errors_total{operation, error_category}`. The SPI outcome
/// is returned unchanged; metric emission is fire-and-forget and never mutates
/// or reorders the result. A free function (not a `Service` method) so it is
/// reusable inside the concurrent fan-out closures of the batch path.
///
/// **The plugin-host span (DESIGN §3.11.4).** `fut` is `.instrument`ed with a
/// `plugin_spi_dispatch` span carrying `operation = op.as_str()`, opened
/// around the dispatch alone (not the metric emission above, which runs after
/// the span has closed). W3C trace context propagates on span **parentage**,
/// not attributes, so a span opened here inherits the caller's ambient request
/// span and end-to-end traces run gateway → core → plugin → backend with no
/// further wiring.
///
/// DESIGN §3.11.5's cardinality paragraph is why this carries only
/// `operation`: every identifier that may not become a metric label would need
/// the same discipline on a span attribute, and none is in scope at this seam
/// — `instrument_spi` receives no identifier parameter at all, by design.
// @cpt-algo:cpt-cf-usage-collector-algo-slo-latency-attribution:p1
// @cpt-dod:cpt-cf-usage-collector-dod-slo-latency-attribution-boundary:p1
// Second marker site for the DoD below: its bootstrap and OTLP-push clauses are
// realized at `build_default_adapter` (`infra/metrics.rs`); its
// trace-propagation clause is realized here, by the span parentage described
// above. An identifier realized in two places carries a marker in both.
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

/// Project a per-record ingestion rejection onto the closed §3.11.5
/// `uc_ingestion_records_total.error_category` vocabulary (keyed off the typed
/// `UsageCollectorError` variant + its discriminators, not a wire string).
pub(crate) fn classify_record_error(err: &UsageCollectorError) -> RecordErrorCategory {
    match err {
        UsageCollectorError::PermissionDenied { .. } => RecordErrorCategory::Authz,
        // §3.11.5's `invalidation_rule` covers the target-resolution and copy
        // rules alone. The target rule — a derived identifier resolving to
        // nothing — is a `NotFound`, separated from an ordinary missing entry
        // by the typed `NotFoundReason` rather than by matching caller-facing
        // prose. `UsageRecordNotFound` shares the wildcard's body but keeps an
        // explicit arm: a decided classification, not a case nobody considered.
        #[allow(clippy::match_same_arms)]
        UsageCollectorError::NotFound { reason, .. } => match reason {
            NotFoundReason::DeclarationNotFound => RecordErrorCategory::UnknownUsageType,
            NotFoundReason::InvalidationTargetNotFound => RecordErrorCategory::InvalidationRule,
            NotFoundReason::UsageRecordNotFound => RecordErrorCategory::SemanticsViolation,
            // Required: `NotFoundReason` is `#[non_exhaustive]` and foreign.
            // A future lookup kind falls here silently, so a new variant must
            // be reviewed against §3.11.5 and given an explicit arm above.
            _ => RecordErrorCategory::SemanticsViolation,
        },
        UsageCollectorError::InvalidArgument { reason, .. } => match reason {
            ValidationReason::UnknownMetadataKey | ValidationReason::MetadataValidation => {
                RecordErrorCategory::MetadataSize
            }
            // The copy rule, the one invalidation rule that arrives here as an
            // `InvalidArgument`. DESIGN §3.11.5 gives the invalidation rules a
            // category of their own so a correction backlog is legible without
            // reading `detail`; target resolution joins it from the `NotFound`
            // arm above, and the two are all the spec scopes the category to.
            ValidationReason::InvalidationFieldMismatch => RecordErrorCategory::InvalidationRule,
            _ => RecordErrorCategory::SemanticsViolation,
        },
        UsageCollectorError::Conflict { outcome, .. } => match outcome {
            // DESIGN §3.11.5 assigns these reasons to DIFFERENT categories in
            // one sentence: "invalidation_rule covers the target-resolution
            // and copy rules alone, a target not yet converged included. A
            // second invalidation rejected as already invalidated is
            // idempotency_conflict, like any dedup conflict." Hence the split
            // below: a second invalidation under another reason code is the
            // dedup conflict, not a target-resolution or copy rule.
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
/// `uc_query_requests_total` per usage-query.md `inst-*-telemetry-complete`.
///
/// **Seam note:** `cursor_decode`, `order_mismatch` and
/// `missing_security_context` surface only at the REST boundary, upstream of
/// this service seam, so they stay reserved-not-emitted here.
/// `filter_mismatch` is **not** one of them: the query a continuation is
/// bound to is compared behind the service, by
/// [`crate::domain::list_query::require_cursor_fingerprint`], so this seam is
/// the only place that category can arise. Which category a cursor rejection
/// lands on is read off the `toolkit_odata` error the refusal carries, not
/// off any code this gear originates (Spec §3.13).
///
/// A PDP-transport failure and a plugin fault both surface as
/// `ServiceUnavailable` at this seam and both map to `plugin_error`; the
/// authoritative PDP-unavailability signal is the foundation-owned
/// `uc_pdp_failures_total`.
///
/// **`ValidationReason::CursorBeyondRetention` is reviewed and deliberately
/// given no arm here.** DESIGN §3.11.5 puts the replay-refusal signal on
/// `uc_feed_requests_total{error_category="cursor_beyond_retention"}` (see
/// [`FeedErrorCategory::CursorBeyondRetention`] and `classify_feed_result`),
/// not on this counter; no query path raises it in any case.
pub(crate) fn classify_query_result<T>(
    result: &Result<T, UsageCollectorError>,
) -> (RequestOutcome, QueryErrorCategory) {
    match result {
        Ok(_) => (RequestOutcome::Success, QueryErrorCategory::None),
        Err(UsageCollectorError::PermissionDenied { .. }) => {
            (RequestOutcome::Denied, QueryErrorCategory::Authz)
        }
        // Discriminated on the typed `reason`, as `classify_record_error`
        // does on the ingestion side. Four callers reach here and they do not
        // share a condition:
        //
        // * `list_usage_records`, `query_aggregated_usage_records` and
        //   `get_reconciliation_metadata` each resolve the queried meter
        //   before dispatch, so each can raise only `DeclarationNotFound`.
        // * `get_usage_record` (`QueryKind::Point`) resolves no declaration
        //   at all and raises `UsageRecordNotFound` — absent or out of scope,
        //   a different condition with a category of its own.
        //
        // **This arm is only safe to read as a genuine miss because the point
        // path routes its PDP denies away before reaching here**:
        // `collapse_deny_to_not_found` makes a deny byte-identical to a miss,
        // and nothing here can read that back out. `get_usage_record`
        // classifies the decision before the collapse runs and substitutes
        // `(denied, authz)` itself. A second path that collapses a deny into
        // this reason without doing the same would relabel denies
        // `record_not_found`.
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
            // A future lookup kind falls here silently, so a new variant must
            // be reviewed against §3.11.5 and given an explicit arm above.
            _ => (RequestOutcome::Error, QueryErrorCategory::UnknownUsageType),
        },
        // A cursor rejection is a continuation refused, and the two conditions
        // behind it do not share a label. The discriminator is the upstream
        // `toolkit_odata` error the variant carries, the same value that
        // decides the wire code (Spec §3.13). `FilterMismatch` is a cursor
        // minted over a different query, not a budget or surface rejection.
        Err(UsageCollectorError::CursorRejected { source, .. }) => match source {
            toolkit_odata::Error::FilterMismatch => {
                (RequestOutcome::Error, QueryErrorCategory::FilterMismatch)
            }
            // Every other cursor rejection is an `INVALID_CURSOR`, folded into
            // `query_budget`. The one known imprecision on this seam, inherited
            // rather than introduced: left as it was so the vocabulary pass
            // does not silently move an operator's series.
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
/// for `uc_feed_requests_total` (DESIGN §3.11.5).
///
/// **A function of its own, never an arm on [`classify_query_result`].** The
/// two vocabularies overlap on four spellings (`none`, `authz`,
/// `cursor_decode`, `plugin_error`) and disagree on the other two this counter
/// carries: `cursor_beyond_retention`, which the query counter has no value
/// for, and `argument_rejected` rather than `query_budget`. Folding the feed
/// into the query classifier would land the retention refusal on
/// `query_budget`, and §3.11's alerting table watches the retention rate
/// specifically, so a refusal counted elsewhere silences the one alert this
/// component publishes.
///
/// The retention refusal arrives as an `InvalidArgument`: the plugin raises
/// [`UsageCollectorPluginError::CursorBeyondRetention`], `DomainError` keeps it
/// whole, and `domain/error.rs`'s lift turns it into a `400` on `cursor`
/// carrying [`ValidationReason::CursorBeyondRetention`]. The typed reason
/// discriminates it here — not a substring of the caller-facing `detail`.
///
/// **A PDP outage is counted as [`FeedErrorCategory::PluginError`], a ruled
/// fold rather than unnamed residue:** a transport failure and a plugin fault
/// both surface as `ServiceUnavailable` at this seam, and `Authz` is reserved
/// for a *completed* decision. Same fold and same remedy as
/// [`classify_query_result`] — `uc_pdp_failures_total` is the authoritative
/// signal — and no §3.11 alert reads `plugin_error` on this counter, so the
/// fold costs no false page.
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
        // The behind-the-service `limit` / subscription-breadth check:
        // `crate::domain::feed::resolve_limit` and
        // `require_subscription_breadth` are the only callers reaching
        // `read_usage_feed` with this variant and reason, the REST edge having
        // already refused a malformed value. A caller-surface argument
        // rejection, not a plugin fault. Matched on the typed reason rather
        // than the bare variant so a future reason added to those call sites
        // does not fold in here silently.
        Err(UsageCollectorError::InvalidArgument { reason, .. })
            if *reason == ValidationReason::Validation =>
        {
            (RequestOutcome::Error, FeedErrorCategory::ArgumentRejected)
        }
        // A catch-all is required here: both `InvalidArgument` arms above are
        // guarded, and a match guard never contributes to exhaustiveness.
        // `ValidationReason` and `UsageCollectorError`
        // (`usage-collector-sdk/src/reason.rs`, `error.rs`) are also
        // `#[non_exhaustive]`, so a reason this seam has never seen falls here
        // and counts as a `plugin_error` silently. A new `ValidationReason`
        // reachable from this seam must be reviewed against §3.11.5 and given
        // an explicit arm above.
        Err(_) => (RequestOutcome::Error, FeedErrorCategory::PluginError),
    }
}

/// Reports, without failing the request, a live feed read the plugin
/// answered with no continuation.
///
/// DESIGN §3.2 fixes the disposition on the *plugin* — a next cursor "with
/// every page of a live read, short pages included" — and no compiler holds
/// a plugin to it. `FeedPage.next` is an `Option<FeedPosition>` on a
/// conforming and a breaching implementation alike, so a plugin that stops
/// minting one recompiles clean and serves a `200`.
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
/// `CursorBeyondRetention` answered to a [`FeedStart::Oldest`] request.
/// There is no cursor parameter on such a request, so the context-free lift
/// (`domain/error.rs`) would raise a `400` field violation naming `cursor`
/// — a parameter the caller never sent — advising them to restart the
/// subscription from its oldest retained position, which is exactly what
/// they did. DESIGN §3.2 forecloses the pair outright: "The refusal reads a
/// caller-supplied cursor only, the implicit start position being by
/// construction the oldest one served."
///
/// So it is lifted as the host-contract breach it is. Two intended
/// consequences: the caller reads a `500` rather than a `400` naming a
/// parameter they never sent, and `classify_feed_result` counts it under
/// `plugin_error` instead of `cursor_beyond_retention`. The second is the
/// substantive one — `cursor_beyond_retention` backs DESIGN §3.11.6's single
/// feed alert, so a misbehaving plugin firing it would page an operator about
/// a consumer that is not behind at all.
///
/// Every other error takes the context-free `From`, unchanged.
///
/// Realizes `cpt-cf-usage-collector-dod-retention-feed-conformance`: the
/// refusal originates at the storage plugin, the only place that reads what
/// it still holds, and this function passes that decision through (relabelling
/// the one breach pair above) rather than re-deriving a retention refusal from
/// a cursor's age.
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
