//! The per-operation structured log entry
//! (`cpt-cf-usage-collector-algo-telemetry-log-correlation`,
//! `cpt-cf-usage-collector-dod-telemetry-log-correlation`).
//!
//! One function, [`log_operation_completed`], is the sole production callsite
//! of the `tracing::info!` this module emits, called from every `Service`
//! operation boundary at the same point each operation's own metric is
//! recorded. The log's vocabulary and the counter's cannot drift apart
//! (`inst-log-shared-vocabulary`): every caller passes the `as_str()` of the
//! very enum ([`crate::domain::ports::metrics`]) it just gave its own
//! `UsageCollectorMetrics` call, never a hand-written literal.
//!
//! # No extraction layer in this gear (ruling I52)
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
//! # The absences (`inst-log-no-metadata`, ruling I51)
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

use crate::domain::ports::metrics::{
    FeedErrorCategory, IngestRequestErrorCategory, IngestRequestOutcome, PdpOp, QueryErrorCategory,
    RequestOutcome,
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "observability_tests.rs"]
mod observability_tests;
