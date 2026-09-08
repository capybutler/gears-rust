//! Domain service for the usage-collector module.
//!
//! The `Service` is the sole owner of the lazy storage-plugin binding.
//! Plugin discovery is resolved on the first dispatch via the embedded
//! `GtsPluginSelector` (single-flight `get_or_init`); the resolved
//! `GtsInstanceId` is cached for the `Service`'s lifetime, so binding
//! changes require a module restart. The structural readiness fact
//! (selector cached AND the scoped `dyn UsageCollectorPluginV1` client is
//! registered in `ClientHub`) is computed per dispatch — the SPI exposes
//! no plugin-side `ready()` probe.
//!
//! Authorization is a direct PDP call per operation through
//! [`crate::domain::authz`]; the resource definitions and action
//! vocabularies all live there so the PEP declarations stay in one place.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::PolicyEnforcer;
use futures::StreamExt;
use futures::stream;
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use toolkit_macros::domain_model;
use toolkit_odata::{CursorV1, ODataQuery, Page as ODataPage, ast};
use toolkit_security::SecurityContext;
use tracing::info;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient, TypesRegistryError};
use usage_collector_sdk::{
    AggregationDimension, AggregationResult, ConflictReason, CreateUsageRecord, EntryType,
    Invalidation, MAX_AGGREGATION_BUCKETS, MetadataFilter, MeterTypeId, RecordOrigin, TimeRange,
    UsageCollectorError, UsageCollectorPluginError, UsageCollectorPluginSpecV1,
    UsageCollectorPluginV1, UsageRecord, ValidationReason,
};
use uuid::Uuid;

use crate::domain::authz::{self, AttributionTupleKey};
use crate::domain::covered_period::{
    CoveredPeriodBounds, enforce_covered_period_bounds, ingestion_action,
};
use crate::domain::invalidation::verify_invalidation_target;
use crate::domain::ports::declarations::UnavailableDeclarationSource;
use crate::domain::ports::metrics::{
    IngestRequestErrorCategory, IngestRequestOutcome, NoopMetrics, PdpOp, PluginErrorCategory,
    PluginOp, QueryErrorCategory, QueryKind, RecordErrorCategory, RecordOutcome, RequestOutcome,
    UsageCollectorMetrics,
};
use crate::domain::query::{
    admit_continuation, compose_query_with_scope, establish_keyset_order, read_fingerprint,
    reject_reserved_filter_fields, require_dimensions_declared,
    require_metadata_filter_keys_declared,
};
use crate::domain::type_resolver::{ResolvedDeclaration, TypeResolver, TypeResolverConfig};
use crate::domain::validation::{DEFAULT_METADATA_SIZE_CAP_BYTES, validate_submit_record_metadata};

use super::error::DomainError;

/// Maximum number of records accepted in a single `create_usage_records`
/// invocation, enforced at the SDK-facing service entry per
/// `cpt-cf-usage-collector-dod-usage-emission-nfr-batch-and-report-timing`.
/// The REST handler is a thin wrapper over this entry, so the cap is the
/// same on both surfaces; `usage-collector-v1.yaml` documents it as
/// `CreateUsageRecordsRequest.records.maxItems` on the wire.
pub const MAX_BATCH_RECORDS: usize = 100;

/// Concurrency cap for the per-distinct-attribution-tuple PDP fan-out in
/// `create_usage_records`. Sized to match the platform's established
/// external-call posture (8) so a worst-case all-distinct
/// [`MAX_BATCH_RECORDS`] batch takes `ceil(100 / 8) × PDP_RTT` wall-clock
/// without overwhelming the PDP transport pool. Bounds the
/// `inst-algo-attrib-bounded-fanout` step of
/// `cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization`.
const PDP_CONCURRENCY: usize = 8;

/// Concurrency cap for the per-distinct-`gts_type_id` Type Resolver fan-out in
/// `create_usage_records`. Bounds request-local pressure on
/// [`TypeResolver::resolve`] (itself single-flighted per key, and normally
/// served from cache — see [`crate::domain::type_resolver`]) for the
/// type-resolution pre-pass; sized identically to [`PDP_CONCURRENCY`] (the
/// three request-local fan-outs — this one, PDP above and the
/// invalidation-target one below — run sequentially, not concurrently, so
/// the effective in-flight ceiling stays at 8). Replaces the retired
/// `CATALOG_FANOUT_CONCURRENCY`, which bounded the plugin-side
/// `get_usage_type` catalog fan-out this pre-pass supersedes.
const TYPE_RESOLUTION_FANOUT_CONCURRENCY: usize = 8;

/// Concurrency cap for the per-distinct-target `get_usage_record` fan-out
/// that resolves a batch's invalidation references in
/// `create_usage_records`. Bounds plugin-side pressure for the
/// target pre-check, and is 8 for the platform's established external-call
/// posture — the same reason the two pre-passes above are 8, restated
/// rather than delegated, so the value survives either of them changing.
/// The three run sequentially, so the effective in-flight ceiling stays at
/// 8. Bounds the `inst-algo-semantics-l1-bounded-fanout` step of
/// `cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2`.
///
/// **A deliberate default, not a pinned one.** No test constrains the
/// number: raising it to `usize::MAX` changes no observable outcome, only
/// how much of the batch is in flight at once. Pinning it would take a
/// test that observes concurrency — overlapping SPI calls against a clock
/// or a barrier — and a timing-dependent test bought to protect a tuning
/// constant is a flake trade this gear is not making. The same is true of
/// the two caps above.
const TARGET_LOOKUP_FANOUT_CONCURRENCY: usize = 8;

/// One PDP fan-out outcome: the input indices that share an attribution
/// tuple plus the `Result<(), DomainError>` returned for that tuple's
/// representative call. Decision projection (success / deny / unavailable
/// → per-index `results[index]` slot) reads this shape.
type PdpGroupDecision = (Vec<usize>, Result<(), DomainError>);

/// Cached resolution per distinct meter, lifted into [`DomainError`] so one
/// resolution outcome projects to every record sharing that type without
/// re-resolving it. Replaces the retired `CatalogCache`, which cached a
/// plugin-owned catalog row per `gts_id` instead of a resolved declaration.
type DeclarationCache = HashMap<MeterTypeId, Result<Arc<ResolvedDeclaration>, DomainError>>;

/// Cached target lookup per distinct `invalidates`, lifted into
/// [`DomainError`] so the variant identity of
/// `UsageRecordNotFound { id }` survives the cache (and is reclassified
/// to `UsageCollectorError::NotFound` on the per-record
/// projection — same lift that the in-loop code path uses).
type InvalidationTargetCache = HashMap<Uuid, Result<UsageRecord, DomainError>>;

/// One entry whose target check was deferred to the post-loop pre-pass.
/// Entries reach it only after passing PDP and the declaration-resolution
/// pre-pass, and only when they carry an `invalidates` reference.
///
/// A named struct rather than a tuple, and that is a correctness choice
/// rather than a stylistic one: two of its four members are record-shaped
/// and two are the pairing itself (the input index the outcome is projected
/// back to, and the reference the fan-out is keyed by). A positional shape
/// with two record-shaped elements is exactly where an alignment slip
/// hides, and a slip in either direction rejects the wrong submission with
/// someone else's identifier.
struct PendingInvalidationTarget {
    /// Input index of the submission, and the only slot in `results` this
    /// entry's outcome may be projected into.
    index: usize,
    /// The submission as the caller sent it. The faithful-copy comparator
    /// runs against this rather than against `record`: the submission has
    /// no identity yet, which is the honest reason `id` is not compared,
    /// and destructuring the *submission* shape is what makes a field added
    /// to [`CreateUsageRecord`] alone a compile error there. Kept because
    /// `CreateUsageRecord::try_into_usage_record` consumes it.
    submission: CreateUsageRecord,
    /// The withdrawal `submission` carries, unwrapped once here so the
    /// verification cannot be reached for an entry that has none. Its
    /// `target` is the fan-out key and the identifier both rejections echo.
    invalidation: Invalidation,
    /// The projected entry, dispatched to the plugin once verified.
    record: UsageRecord,
}

/// Log a host-invariant breach (cache miss, SPI size mismatch, unfilled
/// result slot) and build the typed `Internal` returned for it, so each
/// breach site stays a one-liner and never panics the request thread.
fn invariant_breach(detail: String) -> UsageCollectorError {
    tracing::error!(detail = %detail, "usage-collector host-invariant breach");
    UsageCollectorError::internal(detail)
}

/// Classify a Plugin SPI error for `uc_plugin_accept_errors_total`.
///
/// Only backend-classified faults increment the counter:
/// [`UsageCollectorPluginError::Transient`] / `Internal` → `backend_error`.
/// The deterministic domain-typed variants (`UsageRecord*`,
/// `IdempotencyConflict`) are caller-visible outcomes, **not** plugin faults,
/// and MUST NOT increment it (their duration sample is still recorded): the
/// counter's `error_category` vocabulary in DESIGN §3.11.5 has no value for
/// them. A host-side dispatch deadline (→ `timeout`) does not exist in v1.
fn backend_error_category(err: &UsageCollectorPluginError) -> Option<PluginErrorCategory> {
    match err {
        UsageCollectorPluginError::Transient { .. } | UsageCollectorPluginError::Internal(_) => {
            Some(PluginErrorCategory::BackendError)
        }
        _ => None,
    }
}

/// Plugin-host SPI-dispatch instrumentation wrapper
/// (`cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation`):
/// time a single Plugin SPI call into `uc_plugin_call_duration_seconds{operation}`
/// (success OR error — an error completion is still a dispatch completion) and,
/// on a backend-classified fault, increment
/// `uc_plugin_accept_errors_total{operation, error_category}`. The SPI outcome
/// is returned unchanged; metric emission is fire-and-forget and never mutates
/// or reorders the result. A free function (not a `Service` method) so it is
/// reusable inside the concurrent fan-out closures of the batch path.
// @cpt-algo:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p2
// @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-invoke
async fn instrument_spi<T>(
    metrics: &dyn UsageCollectorMetrics,
    op: PluginOp,
    fut: impl std::future::Future<Output = Result<T, UsageCollectorPluginError>>,
) -> Result<T, UsageCollectorPluginError> {
    let start = std::time::Instant::now();
    let result = fut.await;
    let seconds = start.elapsed().as_secs_f64();
    match &result {
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-duration
        Ok(_) => metrics.record_plugin_call(op, seconds),
        // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-duration
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-catch
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-error-duration
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-error-counter
        Err(e) => {
            metrics.record_plugin_call(op, seconds);
            if let Some(category) = backend_error_category(e) {
                metrics.record_plugin_accept_error(op, category);
            }
        } // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-error-counter
          // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-error-duration
          // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-catch
    }
    // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-return
    // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-error-return
    result
    // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-error-return
    // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-return
}
// @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-invoke

/// RAII guard for the `uc_query_inflight{query_kind}` gauge: increments on
/// [`Self::enter`] (called once authorization composes) and decrements on
/// `Drop` — so the gauge is decremented on *every* exit that followed the
/// increment (including `?` early returns) and never drained without a prior
/// bump, per usage-query.md `inst-*-inflight-increment` / `-telemetry-complete`.
#[domain_model]
struct QueryInflightGuard<'a> {
    metrics: &'a dyn UsageCollectorMetrics,
    kind: QueryKind,
}

impl<'a> QueryInflightGuard<'a> {
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-inflight-increment
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-inflight-increment
    fn enter(metrics: &'a dyn UsageCollectorMetrics, kind: QueryKind) -> Self {
        metrics.query_inflight_inc(kind);
        Self { metrics, kind }
    }
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-inflight-increment
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-inflight-increment
}

impl Drop for QueryInflightGuard<'_> {
    fn drop(&mut self) {
        self.metrics.query_inflight_dec(self.kind);
    }
}

/// `entry_type` label for a submitted entry: `invalidation` iff it names the
/// entry it withdraws, else `record`.
///
/// Reads the submission's own reference for the same reason the domain does
/// — there is no submitted discriminator that could disagree with it, and
/// the sign of a quantity carries no structural meaning
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`). The label type
/// is [`EntryType`] itself, so the value a dashboard groups by is the value
/// the wire carries.
fn entry_type_of(record: &CreateUsageRecord) -> EntryType {
    if record.invalidation.is_some() {
        EntryType::Invalidation
    } else {
        EntryType::Record
    }
}

/// The `operation` label the PDP-helper instruments (`uc_pdp_*`,
/// `uc_authz_decisions_total`) carry for an entry admitted by `origin`'s
/// route.
///
/// The label follows the entry point; the verb an entry is authorized
/// against comes from [`ingestion_action`] and can disagree with it — see
/// [`PdpOp::Backfill`], which owns that distinction.
const fn pdp_op_for(origin: RecordOrigin) -> PdpOp {
    match origin {
        RecordOrigin::Live => PdpOp::Ingest,
        RecordOrigin::Backfill => PdpOp::Backfill,
    }
}

/// Observe `uc_record_metadata_bytes` for a record that carries metadata,
/// measured as the serialized JSON size (the canonical on-the-wire
/// representation the Plugin SPI persists). Records with empty metadata record
/// nothing, matching `inst-algo-metadata-observe-bytes`.
fn observe_metadata_bytes(
    metrics: &dyn UsageCollectorMetrics,
    metadata: &std::collections::BTreeMap<usage_collector_sdk::MetadataKey, String>,
) {
    if metadata.is_empty() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(metadata) {
        metrics.observe_record_metadata_bytes(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }
}

/// Project a per-record ingestion rejection onto the closed §3.11.5
/// `uc_ingestion_records_total.error_category` vocabulary (keyed off the typed
/// `UsageCollectorError` variant + its discriminators, not a wire string).
fn classify_record_error(err: &UsageCollectorError) -> RecordErrorCategory {
    match err {
        UsageCollectorError::PermissionDenied { .. } => RecordErrorCategory::Authz,
        // An unresolved `gts_type_id` (the Type Resolver's `DeclarationNotFound`)
        // vs an `invalidates` naming a missing entry are both wire-tagged
        // `resource_type: USAGE_RECORD_RESOURCE` now that types-registry owns
        // the catalog — `resource_type` can no longer tell them apart. `name`
        // still can: a resolved meter's `gts_type_id` never parses as a `Uuid`
        // and an entry id always does, so that is the discriminator here.
        //
        // The uuid-named arm stays on semantics_violation and does NOT join
        // the invalidation family, even though an unresolvable `invalidates`
        // is the ADR's valid-reference rule. `NotFound` carries no typed
        // reason (that is the variant's shape, not an omission), so the only
        // thing separating it from `usage_record_not_found` is prose in
        // `detail` — and classifying a metric label off a message string is
        // how a label silently stops matching when the message is reworded.
        // §3.11.5's `invalidation_rule` therefore under-counts by exactly
        // this condition; the divergence is recorded rather than paid for
        // with a fragile discriminator.
        UsageCollectorError::NotFound { name, .. } if Uuid::parse_str(name).is_err() => {
            RecordErrorCategory::UnknownUsageType
        }
        UsageCollectorError::NotFound { .. } => RecordErrorCategory::SemanticsViolation,
        UsageCollectorError::InvalidArgument { reason, .. } => match reason {
            ValidationReason::UnknownMetadataKey | ValidationReason::MetadataValidation => {
                RecordErrorCategory::MetadataSize
            }
            // Three of the gateway's five invalidation rules, the three that
            // carry a typed reason: explicit reference (the half-shape the
            // REST fold point refuses), no-invalidation-of-an-invalidation,
            // and faithful copy. DESIGN §3.11.5 gives them a category of
            // their own so a correction backlog is legible without reading
            // `detail`. Valid reference is the fourth and is on
            // `SemanticsViolation` for the reason above; reason code is the
            // fifth and is enforced by the type, so it raises nothing.
            ValidationReason::InvalidationReferenceIncomplete
            | ValidationReason::InvalidationTargetNotRecord
            | ValidationReason::InvalidationFieldMismatch => RecordErrorCategory::InvalidationRule,
            _ => RecordErrorCategory::SemanticsViolation,
        },
        UsageCollectorError::Conflict { reason, .. } => match reason {
            ConflictReason::IdempotencyConflict => RecordErrorCategory::IdempotencyConflict,
            // At-most-one-invalidation, the store's own rule, lifted from the
            // plugin. Same family as the gateway's three above.
            ConflictReason::AlreadyInvalidated => RecordErrorCategory::InvalidationRule,
            _ => RecordErrorCategory::SemanticsViolation,
        },
        _ => RecordErrorCategory::PluginError,
    }
}

/// Project a completed query attempt onto `(outcome, error_category)` for
/// `uc_query_requests_total` per usage-query.md `inst-*-telemetry-complete`.
///
/// **Seam note:** `cursor_decode`, `order_mismatch` and
/// `missing_security_context` surface only at the REST boundary (token
/// decoding, parsing), upstream of this service seam, so they stay
/// reserved-not-emitted here. `filter_mismatch` is **not** one of them any
/// more: the query a continuation is bound to is compared behind the
/// service, by [`require_cursor_fingerprint`], and the REST edge passes
/// `toolkit_odata::validate_cursor_against` no filter hash at all — so this
/// seam is now the only place that category can arise, and it is emitted
/// here. A PDP-transport failure and a plugin fault both surface as
/// `ServiceUnavailable` at this seam and both map to `plugin_error`; the
/// authoritative PDP-unavailability signal is the foundation-owned
/// `uc_pdp_failures_total`.
fn classify_query_result<T>(
    result: &Result<T, UsageCollectorError>,
) -> (RequestOutcome, QueryErrorCategory) {
    match result {
        Ok(_) => (RequestOutcome::Success, QueryErrorCategory::None),
        Err(UsageCollectorError::PermissionDenied { .. }) => {
            (RequestOutcome::Denied, QueryErrorCategory::Authz)
        }
        Err(UsageCollectorError::NotFound { .. }) => {
            (RequestOutcome::Error, QueryErrorCategory::UnknownUsageType)
        }
        // `InvalidArgument` covers two unrelated conditions on the query
        // path, so it discriminates on the typed reason rather than
        // collapsing both into one label.
        //
        // `FILTER_MISMATCH` is a continuation refused because the cursor
        // was minted over a different query: not a budget or surface
        // rejection at all, and it has its own category. Everything else
        // is a query-surface rejection — a `$filter` naming a reserved
        // field, an undeclared `group_by` / `metadata_filter` key, an
        // over-cap aggregate result, or an `$orderby` that cannot be
        // floored into a keyset (mixed directions, or a non-mandatory
        // key), the last of which became reachable here when the keyset
        // floor moved into the domain.
        //
        // One known imprecision, inherited rather than introduced: a
        // continuation whose bound ORDER is not a keyset arrives as
        // `INVALID_CURSOR` and folds into `query_budget` too, which it is
        // not either. Neither `cursor_decode` (a decode failure, genuinely
        // edge-only) nor `order_mismatch` (a caller `$orderby` against the
        // token, which the extractor rejects upstream) describes it, so it
        // needs a category the label vocabulary does not yet carry.
        //
        // The mandatory range cannot land here at all — it is validated
        // where the typed parameter is parsed, at the edge, before the
        // service is entered.
        Err(UsageCollectorError::InvalidArgument { reason, .. }) => (
            RequestOutcome::Error,
            // `expect` rather than `allow`: the duplicate arm below is a
            // placeholder for a category the label vocabulary does not
            // carry yet, so when one is added and the bodies diverge, the
            // lint stops firing and this attribute becomes an unfulfilled
            // expectation the compiler reports. It removes itself.
            #[expect(
                clippy::match_same_arms,
                reason = "the InvalidCursor arm is deliberately explicit; see below"
            )]
            match reason {
                ValidationReason::FilterMismatch => QueryErrorCategory::FilterMismatch,
                // Named where the mapping lives rather than only in the
                // prose above: a continuation whose bound ORDER is not a
                // keyset is not a scan-scope rejection either, but neither
                // `cursor_decode` (a decode failure, genuinely edge-only)
                // nor `order_mismatch` (a caller `$orderby` against the
                // token, rejected upstream by the extractor) describes it,
                // so it folds in here for want of a category that does.
                ValidationReason::InvalidCursor => QueryErrorCategory::QueryBudget,
                _ => QueryErrorCategory::QueryBudget,
            },
        ),
        Err(_) => (RequestOutcome::Error, QueryErrorCategory::PluginError),
    }
}

/// Reports, without failing the request, a `next_cursor` the plugin minted
/// without the fingerprint it was dispatched with.
///
/// Diagnosis only, and deliberately so. The page it accompanies is
/// correct — the rows were selected under the right query — so refusing it
/// would turn a plugin's bookkeeping slip into a failed read. What is
/// broken is the *next* request, which will arrive carrying this token and
/// be refused by [`require_cursor_fingerprint`] with a `400` on the
/// caller's `cursor`. This turns that into an `error!` at the point of
/// breach, one request earlier, naming the component actually at fault.
///
/// Worth a wire decode in the domain — which this layer otherwise leaves
/// to the edge — because the `next_cursor.f` obligation is the one
/// requirement in this gear's Plugin SPI that gives an implementor no
/// compiler error: a plugin written before it recompiles clean and
/// paginates exactly once. The decode diagnoses, never decides; a token
/// that will not decode at all is itself the breach being reported, and an
/// absent `next_cursor` is the ordinary last page.
fn report_unbound_next_cursor<T>(page: &ODataPage<T>, dispatched: Option<&str>) {
    let Some(token) = page.page_info.next_cursor.as_deref() else {
        return;
    };
    let bound = CursorV1::decode(token).ok();
    let bound_fingerprint = bound.as_ref().and_then(|cursor| cursor.f.as_deref());
    if bound_fingerprint == dispatched {
        return;
    }
    tracing::error!(
        bound_fingerprint = bound_fingerprint.unwrap_or("<none>"),
        dispatched_fingerprint = dispatched.unwrap_or("<none>"),
        decoded = bound.is_some(),
        "usage-collector storage plugin minted a next_cursor that does not carry \
         query.filter_hash; the caller's next page will be refused as FILTER_MISMATCH"
    );
}

/// A trivially-true `toolkit_odata` filter: "every row satisfies this."
///
/// `UsageCollectorPluginV1::get_usage_record` now takes a compiled-scope
/// filter on every call, but only `Service::get_usage_record` — the
/// caller-facing point lookup DESIGN §3.3 requires to read under the
/// compiled PDP scope — actually has one to give it. Two call sites in
/// this module use the same SPI method for a system-internal lookup that
/// predates (and is out of scope for) that guarantee, and whose own
/// authorization already happens elsewhere. Both are the same
/// invalidation-target pre-check — a same-request existence / shape check
/// on the entry a submission proposes to withdraw, run *after* the
/// submitting caller's own PDP authorization already succeeded, so not a
/// caller-scoped read of a chosen row — one call site per path:
///
/// * [`resolve_invalidation_targets`], for the batch create path.
/// * [`Service::create_usage_record_inner`], for the single-record one.
///
/// Because the row comes back unscoped, nothing derived from it may reach
/// the caller. [`verify_invalidation_target`] is written to that rule: its
/// rejections name the caller's own reference and the field that differs,
/// never the target's identity or any of its values.
///
/// Passing `true` at both asks the plugin for exactly the "no SPI-level
/// narrowing" behaviour they had before this SPI grew a `scope`
/// parameter — a deliberate, honest "not this surface's scope to give,"
/// not a shortcut around the point lookup's guarantee.
fn unrestricted_read_filter() -> ast::Expr {
    ast::Expr::Value(ast::Value::Bool(true))
}

/// Collapse a PDP denial into `NotFound` so the by-id point lookup
/// (`get`) never acts as an existence oracle; every other error (notably
/// `ServiceUnavailable`, which leaks nothing) is preserved. That lookup
/// is the gear's only by-id surface — a withdrawal is an ordinary
/// ingested entry on the create path, not a second lookup-then-mutate
/// operation — so this has one caller.
fn collapse_deny_to_not_found(
    err: impl Into<UsageCollectorError>,
    id: Uuid,
) -> UsageCollectorError {
    match err.into() {
        UsageCollectorError::PermissionDenied { .. } => {
            UsageCollectorError::usage_record_not_found(id)
        }
        other => other,
    }
}

/// Resolve every deferred invalidation-target check from
/// [`Service::create_usage_records`]'s validation loop.
///
/// Builds a request-local `Map<target, Result<UsageRecord, _>>` via a
/// bounded `get_usage_record` fan-out over the **distinct** targets — a
/// batch withdrawing one entry twice costs one read, not two
/// (`inst-algo-semantics-l1-dedup` /
/// `inst-algo-semantics-l1-bounded-fanout`) — then, for every entry in
/// `pending`, runs [`verify_invalidation_target`] and the deferred metadata
/// check, projecting the outcome into `results` (rejection) or `eligible`
/// (verified).
///
/// The metadata check stays behind the target check so a submission
/// breaking both is told about the copy: the metadata it would be told to
/// fix is metadata it has to copy from the target regardless.
///
/// **The pairing is this function's obligation, in both directions.**
/// [`verify_invalidation_target`] takes the row it is handed and never
/// re-checks that it is the row the entry named, so both directions are
/// checked here.
///
/// Request-local: the fan-out key and the `results` slot are read off one
/// destructured [`PendingInvalidationTarget`], so no entry is verified
/// against a row fetched for a different entry, and no outcome lands on a
/// different input index.
///
/// Store-side: the returned row's `id` must equal the id it was fetched
/// for. That re-derives nothing the store owns — the gateway supplied the
/// id — and it is the same class of check as the SPI result-count breach in
/// [`Service::create_usage_records_inner`]. It has to be here rather than
/// left to the SPI contract, because the failure is not confined to
/// wording a rejection badly: a submission that happens to be a faithful
/// copy of the *returned* row would be **accepted**, withdrawing an entry
/// nothing ever checked.
///
/// At-most-one-invalidation is deliberately **not** checked here. Only the
/// store can make that check atomic with the entry it admits; a
/// gateway-side pre-read cannot exclude a concurrent second submission, so
/// it would be a check that fails exactly when it matters
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`). The plugin's
/// `AlreadyInvalidated` is lifted on dispatch instead.
///
/// Extracted from the host body to keep `create_usage_records` under the
/// cognitive-complexity cap without losing the explicit
/// `target → metadata` error-priority ordering described in the algorithm.
// @cpt-algo:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1
// @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-dedup
// @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-bounded-fanout
// @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-lookup
async fn resolve_invalidation_targets(
    plugin: &dyn UsageCollectorPluginV1,
    metrics: &dyn UsageCollectorMetrics,
    pending: Vec<PendingInvalidationTarget>,
    declaration_cache: &DeclarationCache,
    metadata_size_cap_bytes: usize,
    results: &mut [Option<Result<UsageRecord, UsageCollectorError>>],
    eligible: &mut Vec<(usize, UsageRecord)>,
) {
    if pending.is_empty() {
        return;
    }

    let distinct_targets: HashSet<Uuid> = pending
        .iter()
        .map(|entry| entry.invalidation.target)
        .collect();

    let target_cache: InvalidationTargetCache =
        stream::iter(distinct_targets.into_iter().map(|target| async move {
            let outcome = instrument_spi(
                metrics,
                PluginOp::GetUsageRecord,
                plugin.get_usage_record(target, &unrestricted_read_filter()),
            )
            .await
            .map_err(DomainError::from);
            (target, outcome)
        }))
        .buffer_unordered(TARGET_LOOKUP_FANOUT_CONCURRENCY)
        .collect()
        .await;

    for entry in pending {
        let PendingInvalidationTarget {
            index,
            submission,
            invalidation,
            record,
        } = entry;

        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-not-found
        // The pre-pass populates the cache for every pending target, so a
        // missing entry here is a host-invariant breach. Surface it as a
        // typed `Internal` per-record error rather than `unreachable!()` —
        // request paths must not panic on an invariant failure, matching the
        // SPI-size-mismatch arm in `create_usage_records_inner`.
        let target = match target_cache.get(&invalidation.target) {
            Some(Ok(row)) => row,
            Some(Err(DomainError::UsageRecordNotFound { .. })) => {
                results[index] = Some(Err(UsageCollectorError::invalidation_target_not_found(
                    invalidation.target,
                )));
                continue;
            }
            Some(Err(e)) => {
                results[index] = Some(Err(UsageCollectorError::from(e.clone())));
                continue;
            }
            None => {
                results[index] = Some(Err(invariant_breach(format!(
                    "target pre-pass cache miss for invalidates {}",
                    invalidation.target,
                ))));
                continue;
            }
        };
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-not-found

        // The store answered with a different entry than the one it was
        // asked for. A host-invariant breach, not a caller fault: the id
        // was the gateway's to supply and the SPI's to honour. The detail
        // names only the id the caller sent — the row's own identity is
        // exactly what must not cross back on an unscoped read.
        if target.id != invalidation.target {
            results[index] = Some(Err(invariant_breach(format!(
                "storage plugin answered get_usage_record({}) with a different entry",
                invalidation.target,
            ))));
            continue;
        }

        // The comparator is handed the submission, not `record`: the two
        // carry the same caller-supplied fields, but only the submission
        // shape makes a field added to it alone a compile error there.
        if let Err(e) = verify_invalidation_target(&submission, &invalidation, target) {
            results[index] = Some(Err(e));
            continue;
        }

        // Metadata check deferred behind the target check to preserve the
        // error-priority ordering the pre-A3 in-loop code exposed. A missing
        // declaration entry here is a host-invariant breach (the pre-pass
        // covers every PDP-allowed record's gts_type_id); surface it as a
        // typed `Internal` rather than panic the request thread.
        let Some(Ok(declaration)) = declaration_cache.get(&record.gts_type_id) else {
            results[index] = Some(Err(invariant_breach(format!(
                "declaration pre-pass cache miss for gts_type_id {} before the metadata check",
                record.gts_type_id,
            ))));
            continue;
        };
        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
        observe_metadata_bytes(metrics, &record.metadata);
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
        if let Err(e) =
            validate_submit_record_metadata(declaration, &record.metadata, metadata_size_cap_bytes)
        {
            results[index] = Some(Err(e));
            continue;
        }

        eligible.push((index, record));
    }
}
// @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-lookup
// @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-bounded-fanout
// @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-dedup

/// `usage-collector` domain service.
///
/// Discovers the bound storage plugin via `types-registry` and delegates
/// durable state to it. Owns the lazy binding resolution.
// @cpt-state:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2
#[domain_model]
pub struct Service {
    hub: Arc<ClientHub>,

    /// Vendor selector read once at `Gear::init`; changing it requires a
    /// gear restart.
    vendor: String,

    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-component-plugin-host:p2
    selector: GtsPluginSelector,

    /// PEP boundary. The PDP is a hard dependency per
    /// `cpt-cf-usage-collector-adr-pdp-centric-authorization`; the host
    /// fails init if no resolver client is registered, so this field is
    /// always populated at runtime.
    enforcer: PolicyEnforcer,

    /// Operational-metrics sink. Injected at gear bootstrap via
    /// [`Service::new_with_metrics`]; [`Service::new`] defaults it to a
    /// no-op adapter for tests and pre-init contexts. The concrete
    /// OTLP-backed adapter lives in [`crate::infra::metrics`]; the domain
    /// depends only on the [`UsageCollectorMetrics`] port.
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-observability-plugin-host-instruments:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-observability-pdp-helper-instruments:p1
    metrics: Arc<dyn UsageCollectorMetrics>,

    /// Resolves a meter's GTS type declaration (fold, canonical unit,
    /// metadata surface) through a local TTL cache in front of
    /// `types-registry`, so ingestion's own NFRs stay independent of a
    /// second gear's availability and latency. Consulted by
    /// [`Self::query_aggregated_usage_records`] to serve the declared fold,
    /// and by the ingestion paths ([`Self::create_usage_record_inner`] /
    /// [`Self::create_usage_records_inner`]) to validate a submission's
    /// metadata against the declared closed surface — see
    /// [`crate::domain::type_resolver`].
    type_resolver: Arc<TypeResolver>,

    /// Cap on an entry's serialized metadata map, in bytes, enforced by
    /// [`validate_submit_record_metadata`] on every ingestion path.
    /// [`Self::new_with_metrics`] takes this as a plain mandatory `usize` —
    /// it has no default of its own. [`Self::new`] is the one place a
    /// default applies: it has no cap parameter at all and hard-codes
    /// [`DEFAULT_METADATA_SIZE_CAP_BYTES`] when it delegates to
    /// `new_with_metrics`. Production bootstrap (`module.rs`) instead
    /// threads `UsageCollectorConfig::metadata_size_cap_bytes` through
    /// explicitly.
    metadata_size_cap_bytes: usize,

    /// The covered-period bounds both ingestion paths enforce, projected
    /// from the configured `[usage_collector]` block by
    /// `UsageCollectorConfig::covered_period_bounds`. Held as the finished
    /// [`CoveredPeriodBounds`] rather than as the whole config, for the
    /// same reason `metadata_size_cap_bytes` is held as a plain `usize`.
    covered_period_bounds: CoveredPeriodBounds,
}

impl Service {
    /// Storage-plugin resolution is lazy: no `types-registry` query happens
    /// here, it is deferred to the first dispatch.
    ///
    /// Metrics default to a no-op adapter — production wires the real
    /// OTLP-backed adapter through [`Service::new_with_metrics`]. The Type
    /// Resolver defaults to [`UnavailableDeclarationSource`]: this
    /// constructor has no `types-registry` adapter to build one from without
    /// reintroducing the domain → infra edge `DeclarationSource` exists to
    /// prevent (see [`Service::new_with_metrics`]), and nothing consults the
    /// resolver yet, so a permanently-unavailable placeholder is inert in
    /// practice — the TTL/capacity below are irrelevant for the same reason
    /// (a source that never succeeds never populates the cache). Production
    /// bootstrap always goes through [`Service::new_with_metrics`] with a
    /// genuine adapter-backed resolver instead. The metadata size cap is
    /// likewise hard-coded to [`DEFAULT_METADATA_SIZE_CAP_BYTES`] here —
    /// `new_with_metrics` itself takes the cap as a plain mandatory `usize`
    /// with no default; production bootstrap passes
    /// `UsageCollectorConfig::metadata_size_cap_bytes` explicitly instead.
    ///
    /// The covered-period bounds are likewise defaulted here, to
    /// [`CoveredPeriodBounds::default`] — which reads the same three
    /// domain constants `UsageCollectorConfig`'s own defaults read, so the
    /// published values live in exactly one place and a deployment that
    /// moves them cannot leave this constructor behind.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String, enforcer: PolicyEnforcer) -> Self {
        let metrics: Arc<dyn UsageCollectorMetrics> = Arc::new(NoopMetrics);
        let type_resolver = Arc::new(TypeResolver::new(
            Arc::new(UnavailableDeclarationSource),
            TypeResolverConfig {
                ttl: Duration::from_secs(1),
                capacity: 1,
            },
            Arc::clone(&metrics),
        ));
        Self::new_with_metrics(
            hub,
            vendor,
            enforcer,
            metrics,
            type_resolver,
            DEFAULT_METADATA_SIZE_CAP_BYTES,
            CoveredPeriodBounds::default(),
        )
    }

    /// Construct the service with an explicit operational-metrics sink, a
    /// pre-built Type Resolver, and the configured metadata size cap.
    ///
    /// Used at gear bootstrap (`module.rs`), which builds the resolver via
    /// [`crate::infra::types_registry_source::build_default_resolver`] over
    /// the configured `[usage_collector]` cache knobs and passes
    /// `UsageCollectorConfig::metadata_size_cap_bytes` verbatim as
    /// `metadata_size_cap_bytes`, and by tests that need a real metrics
    /// adapter, a resolver over a fake `DeclarationSource`, or a non-default
    /// size cap — build the resolver with [`TypeResolver::new`] and pass it
    /// in directly, the way `service_with_metrics` (test-only) does for
    /// metrics.
    ///
    /// Taking the finished `Arc<TypeResolver>` here, rather than raw cache
    /// knobs or a `DeclarationSource`, keeps this domain module free of any
    /// dependency on the concrete `types-registry` adapter — mirrors how
    /// `metrics` is injected as a finished `Arc<dyn UsageCollectorMetrics>`
    /// rather than built from a prefix string in here. `metadata_size_cap_bytes`
    /// is likewise taken as the plain `usize` the config carries (not the
    /// whole `UsageCollectorConfig`), for the same reason — and so is
    /// `covered_period_bounds`, taken as the finished
    /// [`CoveredPeriodBounds`] that
    /// `UsageCollectorConfig::covered_period_bounds` projects.
    #[must_use]
    pub fn new_with_metrics(
        hub: Arc<ClientHub>,
        vendor: String,
        enforcer: PolicyEnforcer,
        metrics: Arc<dyn UsageCollectorMetrics>,
        type_resolver: Arc<TypeResolver>,
        metadata_size_cap_bytes: usize,
        covered_period_bounds: CoveredPeriodBounds,
    ) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            enforcer,
            metrics,
            type_resolver,
            metadata_size_cap_bytes,
            covered_period_bounds,
        }
    }

    /// The first two steps of both ingestion paths: project the submission
    /// into its persisted shape, then admit or refuse its covered period.
    ///
    /// One function rather than a copy per path, because the two are
    /// obliged to agree and a second spelling is how they stop agreeing —
    /// the batch path in particular judges every entry of one submission
    /// against a single `now`, and taking that instant as a parameter is
    /// what makes it structural rather than conventional. Both failures are
    /// per-submission: the projection's own period preconditions
    /// (`cpt-cf-usage-collector-adr-record-identity-derivation` — a bound
    /// finer than the microsecond, or an inverted period) and the path's
    /// tolerances alike surface at one entry, never at the batch.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::InvalidArgument`] from the projection, on a
    ///   sub-microsecond or inverted covered period.
    /// * [`UsageCollectorError::InvalidArgument`] with reason
    ///   `FUTURE_WINDOW` / `PAST_WINDOW` when the period ends outside this
    ///   path's tolerances — see [`enforce_covered_period_bounds`].
    fn project_and_admit(
        &self,
        submission: CreateUsageRecord,
        origin: RecordOrigin,
        now: OffsetDateTime,
    ) -> Result<UsageRecord, UsageCollectorError> {
        let record = submission.try_into_usage_record(origin)?;
        enforce_covered_period_bounds(&self.covered_period_bounds, origin, now, record.window_end)?;
        Ok(record)
    }

    /// Create a single `UsageRecord` through the ingestion path per
    /// `cpt-cf-usage-collector-flow-usage-emission-emit-record`. No
    /// in-process catalog cache — the referenced meter's declaration is
    /// resolved through the Type Resolver on each call (itself
    /// TTL-cached — see [`crate::domain::type_resolver`]), not read from a
    /// plugin-owned catalog.
    ///
    /// `origin` is the caller's route, not a caller's value: the wrapper
    /// that *is* a route passes its own, which is the only thing
    /// distinguishing the live and backfill entry points over this shared
    /// pipeline.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::PermissionDenied`] /
    ///   [`UsageCollectorError::ServiceUnavailable`] when the PDP denies or
    ///   is unavailable.
    /// * [`UsageCollectorError::NotFound`] when the referenced `gts_type_id`
    ///   does not resolve to a usable declaration.
    /// * [`UsageCollectorError::InvalidArgument`] on a rejected covered
    ///   period — a bound finer than microsecond precision, or an inverted
    ///   period. This is raised by the projection in the first statement of
    ///   the body, so it outranks every other failure here.
    /// * [`UsageCollectorError::InvalidArgument`] with reason
    ///   `FUTURE_WINDOW` / `PAST_WINDOW` when the covered period ends
    ///   outside this path's tolerances
    ///   ([`enforce_covered_period_bounds`]). Raised immediately after the
    ///   projection, so it outranks everything below it — including the PDP
    ///   call, per DESIGN §3.8's pipeline order.
    /// * [`UsageCollectorError::InvalidArgument`] on a malformed `metadata`
    ///   payload or a semantics violation.
    /// * Any other [`UsageCollectorError`] variant lifted from a plugin
    ///   transport / persistence failure.
    // @cpt-flow:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-ingestion:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-record-metadata:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-resource-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-subject-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-ingestion-authorization:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-usage-type-existence-and-semantics:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-tenant-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-principle-fail-closed:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-principle-pluggable-storage:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-constraint-no-business-logic:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-component-ingestion-gateway:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-entity-usage-type:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-idempotency:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-principle-idempotency-by-key:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-adr-mandatory-idempotency:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-adr-caller-supplied-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-seq-emit-usage:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-entity-usage-record:p1
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-submit
    // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-missing-ctx
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-receive-ctx
    async fn create_usage_record_inner(
        &self,
        ctx: &SecurityContext,
        record: CreateUsageRecord,
        origin: RecordOrigin,
    ) -> Result<UsageRecord, UsageCollectorError> {
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-receive-ctx
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-missing-ctx
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-submit
        // The service is the guaranteed choke point for every caller (REST +
        // in-process). The create surface is identity-free
        // (`CreateUsageRecord`); the entry acquires its deterministic
        // dedup-identity-derived `id` HERE, and only after its covered
        // period has been validated —
        // `cpt-cf-usage-collector-adr-record-identity-derivation` requires
        // both period preconditions to be rejected before the derivation
        // runs.
        //
        // The projection consumes the submission, and the faithful-copy
        // comparator runs against the submission rather than the projection
        // — so an entry naming a target keeps what the caller sent. The
        // clone is confined to that branch: an ordinary measurement, the
        // common path, clones nothing.
        let withdrawal = record
            .invalidation
            .as_ref()
            .map(|invalidation| (record.clone(), invalidation.clone()));
        // One clock read for the admission and the action alike: the two
        // read the same `window_end` against bounds that share an origin,
        // and a second `now_utc()` could put them on opposite sides of the
        // backfill window.
        let now = OffsetDateTime::now_utc();
        // Ahead of the PDP call below, because §3.8 orders period
        // validation (step 3) before authorization (step 5).
        let record = self.project_and_admit(record, origin, now)?;
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-attrib-authz
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-pdp-deny
        authz::authorize_usage_record(
            &self.enforcer,
            self.metrics.as_ref(),
            pdp_op_for(origin),
            ctx,
            &record,
            ingestion_action(&self.covered_period_bounds, origin, now, record.window_end),
        )
        .await
        .map_err(UsageCollectorError::from)?;
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-pdp-deny
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-attrib-authz

        let plugin = self
            .resolve_plugin_for(PluginOp::CreateUsageRecord)
            .await
            .map_err(UsageCollectorError::from)?;

        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-usage-type-not-found
        // Resolve the referenced meter's declaration through the Type
        // Resolver (not the plugin — there is no in-process catalog cache,
        // and validation no longer reads a plugin-owned catalog row at
        // all). An unresolvable type fails closed here, before any plugin
        // dispatch, mirroring `Self::query_aggregated_usage_records`'s
        // identical fail-closed posture on the read path.
        let declaration = self.type_resolver.resolve(&record.gts_type_id).await?;
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-usage-type-not-found

        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-semantics-check
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-semantics-invalid
        // `invalidates` is the whole decision: its presence is what makes
        // the entry an invalidation, and there is no submitted
        // discriminator that could disagree with it. An ordinary
        // measurement costs no target read at all — asserting that absence
        // is what stops the common path paying for the rare one.
        if let Some((submission, invalidation)) = withdrawal {
            // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-lookup
            let target = match instrument_spi(
                self.metrics.as_ref(),
                PluginOp::GetUsageRecord,
                plugin.get_usage_record(invalidation.target, &unrestricted_read_filter()),
            )
            .await
            {
                Ok(row) => row,
                // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-not-found
                Err(UsageCollectorPluginError::UsageRecordNotFound { .. }) => {
                    return Err(UsageCollectorError::invalidation_target_not_found(
                        invalidation.target,
                    ));
                }
                // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-not-found
                Err(e) => return Err(UsageCollectorError::from(DomainError::from(e))),
            };
            // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-lookup
            // The store answered with a different entry than the one it was
            // asked for — the same breach `resolve_invalidation_targets`
            // rejects, and with even less excuse here: this path reads one
            // id and gets one row back.
            if target.id != invalidation.target {
                return Err(invariant_breach(format!(
                    "storage plugin answered get_usage_record({}) with a different entry",
                    invalidation.target,
                )));
            }
            verify_invalidation_target(&submission, &invalidation, &target)?;
        }
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-semantics-invalid
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-semantics-check

        // The metadata check below stays after the target check above: a
        // submission breaking both rules is told about the copy, because
        // the metadata it would be told to fix is metadata it has to copy
        // from the target regardless.

        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
        observe_metadata_bytes(self.metrics.as_ref(), &record.metadata);
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-metadata-closed-shape
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-metadata-cap
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-metadata-too-large
        validate_submit_record_metadata(
            &declaration,
            &record.metadata,
            self.metadata_size_cap_bytes,
        )?;
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-metadata-too-large
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-metadata-cap
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-metadata-closed-shape

        // @cpt-begin:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2:inst-state-usage-record-validated
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-spi-dispatch
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-spi-catch
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-spi-fail
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-conflict
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-accepted
        // @cpt-begin:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2:inst-state-usage-record-persisted
        // @cpt-begin:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2:inst-state-usage-record-spi-error
        // @cpt-begin:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2:inst-state-usage-record-rejected-validation
        instrument_spi(
            self.metrics.as_ref(),
            PluginOp::CreateUsageRecord,
            plugin.create_usage_record(record),
        )
        .await
        .map_err(|e| UsageCollectorError::from(DomainError::from(e)))
        // @cpt-end:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2:inst-state-usage-record-rejected-validation
        // @cpt-end:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2:inst-state-usage-record-spi-error
        // @cpt-end:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2:inst-state-usage-record-persisted
        // @cpt-end:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2:inst-state-usage-record-validated
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-accepted
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-conflict
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-spi-fail
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-spi-catch
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-spi-dispatch
    }

    /// Single-emit ingestion entry
    /// (`cpt-cf-usage-collector-flow-usage-emission-emit-record`): wraps
    /// [`Self::create_usage_record_inner`] with the single-emit completion
    /// telemetry. Per DESIGN §3.11.5 the single-emit SDK surface records
    /// `uc_ingestion_duration_seconds` plus exactly one
    /// `uc_ingestion_records_total` (the request-level `uc_ingestion_requests_total`
    /// is a batch-only counter and is NOT incremented here). Both carry
    /// `origin="live"`: this is the live route, and DESIGN §3.3 declares no
    /// single-emit backfill counterpart.
    ///
    /// # Errors
    ///
    /// Surfaces the same [`UsageCollectorError`] variants as
    /// [`Self::create_usage_record_inner`] — a rejected covered period, PDP
    /// denial, an unresolvable declaration, semantics / metadata
    /// validation, idempotency conflict, or plugin fault.
    pub async fn create_usage_record(
        &self,
        ctx: &SecurityContext,
        record: CreateUsageRecord,
    ) -> Result<UsageRecord, UsageCollectorError> {
        let start = std::time::Instant::now();
        let entry_type = entry_type_of(&record);
        // `Live` comes from the route this wrapper *is*, not from a
        // default: the batch routes stamp their own origin through their
        // own shared body (`create_usage_records_for_origin`), and there is
        // no single-emit backfill counterpart to reach this one. It is also
        // the `origin` label on both ingestion instruments below, so the
        // stamp and the telemetry cannot disagree.
        let origin = RecordOrigin::Live;
        let result = self.create_usage_record_inner(ctx, record, origin).await;
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-completion-metrics
        self.metrics
            .observe_ingestion_duration(start.elapsed().as_secs_f64(), origin);
        let (outcome, error_category) = match &result {
            Ok(_) => (RecordOutcome::Accepted, RecordErrorCategory::None),
            Err(e) => (RecordOutcome::Rejected, classify_record_error(e)),
        };
        self.metrics
            .record_ingestion_record(outcome, entry_type, origin, error_category);
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-record:p1:inst-emit-record-completion-metrics
        result
    }

    /// Batch ingestion entry
    /// (`cpt-cf-usage-collector-flow-usage-emission-emit-records-batch`).
    ///
    /// The live batch route. Enforces the `1..=`[`MAX_BATCH_RECORDS`]
    /// structural cap (rejected before the pipeline and NOT recorded on
    /// either ingestion instrument, per §3.11.5's closed vocabulary),
    /// observes `uc_ingestion_batch_size`, delegates to
    /// [`Self::create_usage_records_inner`], and records the completion
    /// telemetry: one `uc_ingestion_records_total` per per-record outcome,
    /// one `uc_ingestion_requests_total` (`accepted` / `partial` /
    /// `rejected`), and `uc_ingestion_duration_seconds` — the first and last
    /// of those carrying `origin="live"`.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::InvalidArgument`] when the input violates the
    ///   `1..=`[`MAX_BATCH_RECORDS`] cap.
    /// * [`UsageCollectorError::ServiceUnavailable`] / other variants for a
    ///   batch-level plugin transport / persistence failure.
    ///
    /// # Post-condition
    ///
    /// On `Ok`, the returned vector has length equal to `records.len()` and
    /// preserves input order.
    // @cpt-flow:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1
    pub async fn create_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        // The batch body lives in `create_usage_records_for_origin`, shared
        // with the backfill route so the two cannot drift. `Live` comes from
        // the route this wrapper *is*, not from a default.
        self.create_usage_records_for_origin(ctx, records, RecordOrigin::Live)
            .await
    }

    /// Bulk historical import of periods the live path rejects
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`).
    ///
    /// The ADR's *workload* isolation is **not implemented**: this route
    /// shares the live path's runtime, connection pool and fan-out budget,
    /// so a bulk import can still degrade live ingestion p95. See the TODO
    /// on this method's source. What the route does own is its origin
    /// marker, its covered-period bounds and its own PDP labelling.
    ///
    /// Stamps `origin = backfill` and admits the covered periods the live
    /// past tolerance rejects. Validation is otherwise identical to
    /// [`Self::create_usage_records`] — the future tolerance included,
    /// because this route lifts the past bound and nothing else.
    ///
    /// It takes invalidation entries as well as measurements, mixed in one
    /// batch. A withdrawal of a period older than the live past tolerance
    /// belongs here rather than on the live path, so a correction of closed
    /// history reads as history: `origin` records the route each entry
    /// travelled, and it is not one of the fields the faithful-copy rule
    /// compares, so withdrawing a `live` target here is the ordinary case
    /// rather than a mismatch.
    ///
    /// An entry whose covered period ends further back than the configured
    /// backfill window is authorized against
    /// `usage_record::actions::BACKFILL` instead of `CREATE`
    /// ([`ingestion_action`]). One batch may mix the two; every entry of
    /// it is labelled `operation="backfill"` on the PDP instruments either
    /// way — see [`PdpOp::Backfill`], which owns that collision.
    ///
    // TODO(`cpt-cf-usage-collector-nfr-workload-isolation`): this route
    // shares the live path's runtime, connection pool and fan-out budget —
    // it is the same `create_usage_records_for_origin` body under a
    // different `origin`, and nothing here bounds it separately. The ADR
    // makes workload isolation a gear-level obligation and it is
    // unimplemented; a bulk import can still degrade live ingestion p95.
    // Backend pool isolation is separately a plugin deployment obligation.
    // Confirmation is a concurrent load test against
    // `cpt-cf-usage-collector-nfr-throughput-profile`, which is why nothing
    // in the gear-level suite goes red while this stands.
    ///
    /// # Errors
    ///
    /// The same variants as [`Self::create_usage_records`].
    ///
    /// # Post-condition
    ///
    /// As [`Self::create_usage_records`]: on `Ok`, one result slot per
    /// input, in input order.
    pub async fn backfill_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        // `Backfill` comes from the route this wrapper *is*. Everything
        // else — the structural cap, the pipeline, the completion telemetry
        // — is the live route's own body, which is what keeps the two from
        // drifting.
        self.create_usage_records_for_origin(ctx, records, RecordOrigin::Backfill)
            .await
    }

    /// The batch ingestion body, parameterized by the path that admitted the
    /// submission: the structural `1..=`[`MAX_BATCH_RECORDS`] cap, the
    /// pipeline call, and the completion telemetry alike.
    ///
    /// DESIGN §3.2 makes the backfill path "the same component under
    /// workload isolation: identical validation ... and `origin =
    /// backfill`", and §3.3 gives `backfill_usage_records` the same
    /// signature as [`Self::create_usage_records`]. `origin` is therefore
    /// the whole difference between the two batch entry points, and they
    /// share this body rather than each carrying a copy of the completion
    /// telemetry — a second copy is how the two paths' counters drift apart
    /// the first time one of them is edited.
    async fn create_usage_records_for_origin(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
        origin: RecordOrigin,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        let start = std::time::Instant::now();
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-cap-check
        let actual = records.len();
        if actual == 0 || actual > MAX_BATCH_RECORDS {
            // Structural rejection before the pipeline — NOT recorded (the
            // §3.11.5 error_category vocabulary carries no structural category).
            return Err(UsageCollectorError::invalid_batch_size(
                actual,
                1,
                MAX_BATCH_RECORDS,
            ));
        }
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-cap-check

        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-observe-batch-size
        self.metrics
            .observe_ingestion_batch_size(u64::try_from(actual).unwrap_or(u64::MAX));
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-observe-batch-size

        // `entry_type` is captured per input index before `records` is moved
        // into the inner pipeline (the per-entry counter needs it after).
        let entry_types: Vec<EntryType> = records.iter().map(entry_type_of).collect();

        let result = self.create_usage_records_inner(ctx, records, origin).await;
        let seconds = start.elapsed().as_secs_f64();

        match &result {
            Ok(per_record) => {
                // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-records-counter
                for (record_result, entry_type) in
                    per_record.iter().zip(entry_types.iter().copied())
                {
                    let (outcome, error_category) = match record_result {
                        Ok(_) => (RecordOutcome::Accepted, RecordErrorCategory::None),
                        Err(e) => (RecordOutcome::Rejected, classify_record_error(e)),
                    };
                    self.metrics.record_ingestion_record(
                        outcome,
                        entry_type,
                        origin,
                        error_category,
                    );
                }
                // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-records-counter
                // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-request-completion-metrics
                let request_outcome = if per_record.iter().any(Result::is_err) {
                    IngestRequestOutcome::Partial
                } else {
                    IngestRequestOutcome::Accepted
                };
                self.metrics
                    .record_ingestion_request(request_outcome, IngestRequestErrorCategory::None);
            }
            Err(_) => {
                // Whole-request plugin failure (`inst-emit-batch-spi-fail-mark`);
                // the structural cap-check already returned above unrecorded.
                self.metrics.record_ingestion_request(
                    IngestRequestOutcome::Rejected,
                    IngestRequestErrorCategory::PluginError,
                );
            }
        }
        self.metrics.observe_ingestion_duration(seconds, origin);
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-request-completion-metrics
        result
    }

    /// Create a batch of `UsageRecord`s through the ingestion path per
    /// `cpt-cf-usage-collector-flow-usage-emission-emit-records-batch`.
    ///
    /// Per-record stages mirror [`Self::create_usage_record`] and run
    /// independently for each input; eligible records carry their
    /// caller-supplied covered period through to persistence UTC-normalized
    /// but otherwise unchanged — nothing is quantized or truncated — and
    /// are dispatched together. Per-record validation / SPI failures
    /// surface in the result vector at their input index — including a
    /// rejected covered period, which
    /// `cpt-cf-usage-collector-adr-record-identity-derivation` makes a
    /// per-submission precondition of the identity derivation rather than a
    /// batch-level one. The outer `Err` is reserved for batch-level
    /// failures (plugin handle resolution, outer SPI dispatch, and the
    /// structural batch-size cap below).
    ///
    /// The SDK-facing batch cap of `1..=`[`MAX_BATCH_RECORDS`] is enforced
    /// here (not at the REST handler, which is a thin wrapper over this
    /// entry): an empty submission OR a submission exceeding
    /// [`MAX_BATCH_RECORDS`] surfaces as
    /// [`UsageCollectorError::InvalidArgument`] — the canonical lift
    /// renders it as the structural-validation `Problem` envelope (HTTP 400).
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::InvalidArgument`] when the input violates
    ///   the `1..=`[`MAX_BATCH_RECORDS`] cap.
    /// * [`UsageCollectorError::ServiceUnavailable`] when the storage plugin
    ///   handle cannot be resolved or the outer SPI dispatch fails.
    /// * Any other [`UsageCollectorError`] variant lifted from a batch-level
    ///   plugin transport / persistence failure.
    ///
    /// Per-record failures (a rejected covered period, authorization
    /// denial, an unresolvable declaration, malformed metadata, SPI errors
    /// against individual records) surface in the per-index `Result`
    /// entries of the returned vector rather than the outer `Err`. "A
    /// rejected covered period" covers both of
    /// [`Self::project_and_admit`]'s refusals — the projection's own
    /// sub-microsecond / inverted preconditions, and the path's
    /// `FUTURE_WINDOW` / `PAST_WINDOW` tolerances — and every entry of one
    /// batch is judged against a single `now`, so two entries carrying the
    /// same period cannot be decided differently.
    ///
    /// `origin` is the caller's route, not a caller's value: the wrapper
    /// that *is* a route passes its own, which is the only thing
    /// distinguishing the live and backfill entry points over this shared
    /// pipeline. It is one value for the whole batch — a batch is admitted
    /// by one route.
    ///
    /// # Post-condition
    ///
    /// On `Ok`, the returned vector has length equal to `records.len()` and
    /// preserves input order: index `i` of the output corresponds to index
    /// `i` of the input batch.
    //
    // Realizes the batch flow `cpt-cf-usage-collector-flow-usage-emission-emit-records-batch`;
    // DoDs already attributed to the file via `create_usage_record` above
    // (fr-ingestion, fr-ingestion-authorization, fr-record-metadata,
    // fr-usage-type-existence-and-semantics, principle-fail-closed,
    // principle-pluggable-storage, component-ingestion-gateway, seq-emit-usage)
    // are not re-declared here (one `@cpt-dod` per id per file). Workload-
    // isolation is batch-specific so its marker lands here. That DoD is the
    // write-vs-read isolation obligation, which this body satisfies — the
    // gateway is still the sole write entry point and shares no state with
    // the query gateway. The backfill-vs-live gap is a DIFFERENT obligation,
    // owned by no feature file, and it is the TODO at
    // `Self::backfill_usage_records`; do not read this marker as covering it.
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-nfr-workload-isolation:p1
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-receive-ctx
    async fn create_usage_records_inner(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
        origin: RecordOrigin,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-receive-ctx
        // The `1..=MAX_BATCH_RECORDS` cap is enforced by the public
        // `create_usage_records` wrapper (before the batch-size observation),
        // so callers of the inner path are already in range.

        let submission_count = records.len();
        let mut results: Vec<Option<Result<UsageRecord, UsageCollectorError>>> =
            (0..submission_count).map(|_| None).collect();
        // Per-input-index admissibility, cleared by any pass that finishes a
        // slot: the covered-period conversion below and the PDP projection
        // further down.
        let mut pdp_allowed: Vec<bool> = vec![true; submission_count];

        // The withdrawal each submission carries, kept per input index
        // because `try_into_usage_record` consumes the submission the
        // faithful-copy comparator runs against. `Some` exactly when the
        // entry is an invalidation.
        //
        // What this buys is that the trigger is read off the **submission**
        // rather than off the projection, so the comparator runs against
        // what the caller actually sent. The `Invalidation` half is
        // redundant with the one the projected entry carries — it is only
        // ever built from the same submission, so it is never built
        // inconsistently; that is a property of this one construction site,
        // not of the type. The clone is confined to the invalidation branch
        // — an ordinary measurement clones nothing.
        let mut withdrawals: Vec<Option<(CreateUsageRecord, Invalidation)>> =
            Vec::with_capacity(submission_count);

        // The service is the guaranteed choke point for every caller (REST +
        // in-process). The create surface is identity-free
        // (`CreateUsageRecord`); each entry acquires its deterministic
        // dedup-identity-derived `id` HERE, before authorization or
        // dispatch — the single point of derivation.
        //
        // The derivation is per-submission and fallible (the covered-period
        // preconditions of
        // `cpt-cf-usage-collector-adr-record-identity-derivation`), so a bad
        // period surfaces at its own input index instead of failing the
        // batch. Every later pass carries the input index explicitly rather
        // than re-`enumerate()`ing, because the surviving vector is no
        // longer index-aligned with the input.
        //
        // `now` is captured once for the whole batch rather than per entry:
        // a batch is one submission, and two entries carrying the same
        // covered period must not be judged differently because the clock
        // crossed the bound between them.
        let now = OffsetDateTime::now_utc();
        let mut derived: Vec<(usize, UsageRecord)> = Vec::with_capacity(submission_count);
        for (index, submission) in records.into_iter().enumerate() {
            withdrawals.push(
                submission
                    .invalidation
                    .as_ref()
                    .map(|invalidation| (submission.clone(), invalidation.clone())),
            );
            // Per-submission, at its own input index — never a batch-level
            // failure, or one out-of-bounds entry would discard a whole
            // import.
            match self.project_and_admit(submission, origin, now) {
                Ok(record) => derived.push((index, record)),
                Err(e) => {
                    results[index] = Some(Err(e));
                    // Redundant today — every later pass walks `derived`, so
                    // it cannot reach this index anyway — but "this slot is
                    // finished" must not be spelled two different ways. A
                    // pass added later that consults `pdp_allowed` alone
                    // would otherwise read a rejected slot as admissible.
                    pdp_allowed[index] = false;
                }
            }
        }

        let plugin = self
            .resolve_plugin_for(PluginOp::CreateUsageRecords)
            .await
            .map_err(UsageCollectorError::from)?;

        let mut eligible: Vec<(usize, UsageRecord)> = Vec::new();
        let mut pending_targets: Vec<PendingInvalidationTarget> = Vec::new();

        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-pdp
        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-dedup-tuple-key
        let mut distinct_tuples: HashMap<AttributionTupleKey, Vec<usize>> = HashMap::new();
        for (index, record) in &derived {
            // Per record, not per batch: `ingestion_action` reads each
            // entry's own `window_end`, so one backfill batch straddling
            // the configured window carries `create` and `backfill` at
            // once. `action` is part of `AttributionTupleKey`'s hash/eq,
            // which is what keeps two such entries sharing one attribution
            // tuple from collapsing onto a single PDP decision — the entry
            // beyond the window would otherwise ride in on the other's
            // `create` permit.
            let action =
                ingestion_action(&self.covered_period_bounds, origin, now, record.window_end);
            distinct_tuples
                .entry(AttributionTupleKey::from_record(record, action))
                .or_default()
                .push(*index);
        }
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-dedup-tuple-key

        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-bounded-fanout
        // The fan-out passes the tuple `key` (not a `&UsageRecord`) into the
        // PDP composer. This is the load-bearing safety property of the
        // dedup: `authorize_attribution_tuple` has no syntactic access to
        // any record field outside the key, so two records that
        // hash-equal under `AttributionTupleKey` cannot diverge in PDP
        // payload — they share the SAME `AccessRequest` by construction.
        // `action` is part of the key (hash/eq), which is what lets the
        // loop above vary it per record without two verbs collapsing onto
        // one decision.
        let pdp_decisions: Vec<PdpGroupDecision> =
            stream::iter(distinct_tuples.into_iter().map(|(key, indices)| {
                let enforcer = &self.enforcer;
                let metrics = self.metrics.as_ref();
                async move {
                    let decision = authz::authorize_attribution_tuple(
                        enforcer,
                        metrics,
                        pdp_op_for(origin),
                        ctx,
                        &key,
                    )
                    .await;
                    (indices, decision)
                }
            }))
            .buffer_unordered(PDP_CONCURRENCY)
            .collect()
            .await;
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-bounded-fanout

        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-pdp-deny
        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-pdp-allow
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-pdp-projected-deny
        for (indices, decision) in pdp_decisions {
            if let Err(e) = decision {
                // A PDP-transport failure (`AuthorizationUnavailable`) and a
                // plugin `Transient` both lift to `ServiceUnavailable`; their
                // curated `detail` strings keep them distinguishable for
                // operator triage without a separate per-record origin tag.
                for index in indices {
                    results[index] = Some(Err(UsageCollectorError::from(e.clone())));
                    pdp_allowed[index] = false;
                }
            }
        }
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-pdp-projected-deny
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-pdp-allow
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-pdp-deny
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-pdp

        let distinct_gts_type_ids: HashSet<MeterTypeId> = derived
            .iter()
            .filter(|(idx, _)| pdp_allowed[*idx])
            .map(|(_, r)| r.gts_type_id.clone())
            .collect();

        // The fan-out lifts each per-id outcome to `DomainError` (the Type
        // Resolver's own error type) eagerly so the cached value is Clone
        // and a single resolution can be projected to every input index
        // that references the gts_type_id without re-resolving it. Replaces
        // the retired plugin-side `get_usage_type` catalog fan-out at the
        // same bounded concurrency.
        let declaration_cache: DeclarationCache =
            stream::iter(distinct_gts_type_ids.into_iter().map(|gts_type_id| {
                let type_resolver = self.type_resolver.as_ref();
                async move {
                    let outcome = type_resolver.resolve(&gts_type_id).await;
                    (gts_type_id, outcome)
                }
            }))
            .buffer_unordered(TYPE_RESOLUTION_FANOUT_CONCURRENCY)
            .collect()
            .await;

        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-foreach-validate
        for (index, record) in derived {
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-pdp
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-deny
            if !pdp_allowed[index] {
                continue;
            }
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-deny
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-pdp

            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-unknown-usage-type
            // The declaration pre-pass populated `declaration_cache` with a
            // Clone outcome per distinct gts_type_id; every PDP-allowed
            // record's gts_type_id is guaranteed to be present.
            let declaration = match declaration_cache.get(&record.gts_type_id) {
                Some(Ok(decl)) => Arc::clone(decl),
                Some(Err(e)) => {
                    results[index] = Some(Err(UsageCollectorError::from(e.clone())));
                    continue;
                }
                // Host-invariant breach (declaration pre-pass populated by
                // `distinct_gts_type_ids`); typed `Internal` per-record error
                // rather than `unreachable!()` so a future refactor cannot
                // turn an invariant slip into a request-thread panic.
                None => {
                    results[index] = Some(Err(invariant_breach(format!(
                        "declaration pre-pass cache miss for gts_type_id {} during record dispatch",
                        record.gts_type_id,
                    ))));
                    continue;
                }
            };
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-unknown-usage-type

            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-semantics
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-semantics-invalid
            // `invalidates` is the whole decision: its presence is what makes
            // the entry an invalidation, and there is no submitted
            // discriminator that could disagree with it. The target read is
            // deferred to a post-loop dedup + bounded fan-out pre-pass
            // (`inst-algo-semantics-l1-dedup` /
            // `inst-algo-semantics-l1-bounded-fanout`) so a batch withdrawing
            // one target repeatedly reads it once; the metadata check runs
            // after the target check there, so the target→metadata
            // error-priority ordering is preserved end-to-end.
            if let Some((submission, invalidation)) = withdrawals[index].take() {
                pending_targets.push(PendingInvalidationTarget {
                    index,
                    submission,
                    invalidation,
                    record,
                });
                continue;
            }
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-semantics-invalid
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-semantics

            // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
            observe_metadata_bytes(self.metrics.as_ref(), &record.metadata);
            // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-metadata-closed-shape
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-metadata
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-metadata-too-large
            if let Err(e) = validate_submit_record_metadata(
                &declaration,
                &record.metadata,
                self.metadata_size_cap_bytes,
            ) {
                results[index] = Some(Err(e));
                continue;
            }
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-metadata-too-large
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-metadata
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-metadata-closed-shape

            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-eligible
            // (the caller-supplied covered period is carried onto the
            // persisted entry verbatim — `try_into_usage_record` normalized
            // both bounds to UTC and rejected anything finer than the
            // microsecond, so nothing is truncated here or downstream)
            eligible.push((index, record));
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-eligible
        }

        resolve_invalidation_targets(
            plugin.as_ref(),
            self.metrics.as_ref(),
            pending_targets,
            &declaration_cache,
            self.metadata_size_cap_bytes,
            &mut results,
            &mut eligible,
        )
        .await;

        // The target pre-check pushes verified invalidations to `eligible`
        // after the input-order foreach has completed, so the vec is no
        // longer guaranteed in input-index order. Sort once before the
        // plugin SPI dispatch; per-record results are still routed back
        // via the input index, so this only affects the order in which
        // the plugin sees the entries.
        eligible.sort_by_key(|(index, _)| *index);
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-foreach-validate

        if !eligible.is_empty() {
            let (indices, dispatched): (Vec<usize>, Vec<UsageRecord>) =
                eligible.into_iter().unzip();
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-spi-dispatch
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-spi-catch
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-spi-fail-mark
            let spi_results = instrument_spi(
                self.metrics.as_ref(),
                PluginOp::CreateUsageRecords,
                plugin.create_usage_records(dispatched),
            )
            .await
            .map_err(|e| UsageCollectorError::from(DomainError::from(e)))?;
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-spi-fail-mark
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-spi-catch
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-spi-dispatch

            if spi_results.len() != indices.len() {
                return Err(invariant_breach(format!(
                    "plugin returned {} per-record results for {} dispatched records",
                    spi_results.len(),
                    indices.len()
                )));
            }

            // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-foreach-spi
            for (index, spi_result) in indices.into_iter().zip(spi_results) {
                // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-accepted
                // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-conflict
                // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-spi-err
                results[index] =
                    Some(spi_result.map_err(|e| UsageCollectorError::from(DomainError::from(e))));
                // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-spi-err
                // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-conflict
                // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-record-accepted
            }
            // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-emit-records-batch:p1:inst-emit-batch-foreach-spi
        }

        // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-return
        // Every per-record slot is filled by the period / PDP / catalog /
        // semantics / metadata / SPI-fanout passes above; an empty slot here
        // is a host-invariant breach. Yield a typed `Internal` for that slot
        // so the request thread cannot panic.
        Ok(results
            .into_iter()
            .enumerate()
            .map(|(slot_index, opt)| {
                opt.unwrap_or_else(|| {
                    Err(invariant_breach(format!(
                        "per-record slot {slot_index} was not populated before return"
                    )))
                })
            })
            .collect())
        // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization:p1:inst-algo-attrib-return
    }

    /// Read a single `UsageRecord` by `uuid` from the bound storage plugin,
    /// scoped to the caller's compiled PDP grant.
    ///
    /// Authorizes FIRST via [`authz::authorize_get_usage_record_scope`] — a
    /// pre-row PDP request (no per-record attribution attributes; the
    /// id-only boundary doesn't have the record's tenant / resource /
    /// subject fields to offer yet) under `require_constraints(true)`,
    /// mirroring [`Self::list_usage_records`]'s posture. The point lookup
    /// carries no caller-supplied filter, so the projected scope IS the
    /// whole filter passed to Plugin SPI Method 10 `get_usage_record(id,
    /// scope)`: a row outside it is never returned — the plugin reports
    /// `UsageRecordNotFound` exactly as it would for an `id` that doesn't
    /// exist at all, so this surface cannot be used as an existence oracle.
    /// A PDP deny is additionally collapsed into that same `NotFound`
    /// (see [`collapse_deny_to_not_found`]) so a caller denied outright
    /// can't distinguish "denied" from "no matching row" either.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::NotFound`] when the PDP denies (collapsed,
    ///   see above), or when the targeted record does not exist, or exists
    ///   but falls outside the compiled scope (both of the latter reported
    ///   by the plugin as `UsageRecordNotFound`).
    /// * [`UsageCollectorError::ServiceUnavailable`] when the PDP is
    ///   unavailable.
    /// * Any other [`UsageCollectorError`] variant lifted from a plugin
    ///   transport / persistence failure.
    // @cpt-flow:cpt-cf-usage-collector-flow-usage-emission-get-record:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-api-get-records-id:p1
    pub async fn get_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UsageRecord, UsageCollectorError> {
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-pdp
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-pdp-deny
        let scope_expr = authz::authorize_get_usage_record_scope(
            &self.enforcer,
            self.metrics.as_ref(),
            PdpOp::GetRecord,
            ctx,
        )
        .await
        .map_err(|e| collapse_deny_to_not_found(e, id))?;
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-pdp-deny
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-pdp

        let plugin = self
            .resolve_plugin_for(PluginOp::GetUsageRecord)
            .await
            .map_err(UsageCollectorError::from)?;

        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-plugin-dispatch
        let record = instrument_spi(
            self.metrics.as_ref(),
            PluginOp::GetUsageRecord,
            plugin.get_usage_record(id, &scope_expr),
        )
        .await
        .map_err(|e| UsageCollectorError::from(DomainError::from(e)))?;
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-plugin-dispatch

        // @cpt-begin:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-success
        Ok(record)
        // @cpt-end:cpt-cf-usage-collector-flow-usage-emission-get-record:p1:inst-get-record-success
    }

    /// Keyset-paginated list of `UsageRecord`s from the bound storage
    /// plugin's table, narrowed by the PDP-returned constraints.
    ///
    /// Seven responsibilities live here per
    /// `cpt-cf-usage-collector-flow-usage-query-query-raw`:
    ///
    /// 1. **Authorize** the request via [`authz::authorize_list_usage_records`].
    ///    The PEP request is pre-row (no per-record attribution attributes)
    ///    because the caller has not yet named a specific row — the PDP
    ///    responds with row-scope narrowing via the [`AccessScope`]
    ///    constraints, not via a tuple match. It runs under
    ///    `require_constraints(true)`, so the PDP MUST return row-scope
    ///    narrowing (a platform admin resolves to the full tenant set, never
    ///    `allow_all`); a degenerate unconstrained permit is denied in
    ///    composition by [`authz::scope_to_odata_filter`], not read as "all
    ///    tenants".
    /// 2. **Resolve** the queried meter's declaration through
    ///    [`TypeResolver::resolve`], fail-closed, so the admissible-metadata
    ///    gate in step 3 has a declared-keys set to check against. This is
    ///    a behavioural change from before Spec §3.11 gating landed: an
    ///    unresolvable type now surfaces here as a pre-dispatch 404, where
    ///    previously this path never resolved a declaration and dispatched
    ///    straight to the plugin regardless of whether the type was known
    ///    to `types-registry`.
    /// 3. **Gate** the query surface on that declaration (Spec §3.11):
    ///    [`reject_reserved_filter_fields`] rejects a `$filter` naming
    ///    `gts_type_id` or a covered-period bound, and
    ///    [`require_metadata_filter_keys_declared`] rejects a
    ///    `metadata_filter` entry naming a metadata key the declaration
    ///    does not declare (this path takes no `group_by`, so
    ///    [`require_dimensions_declared`] does not apply here).
    /// 4. **Compose** the PDP constraints into the user-supplied `OData`
    ///    filter via [`authz::scope_to_odata_filter`]. The composition is
    ///    intersection-only (`composed = user_filter AND constraints`) per
    ///    `cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2`
    ///    — no widening is permitted. `gts_type_id` and `time_range` stay
    ///    typed parameters and are NOT touched here: neither ever enters
    ///    `query.filter`, which is why a predicate naming a covered-period
    ///    bound is rejected in step 3 rather than merged in here.
    /// 5. **Floor the keyset**, so the plugin always receives the
    ///    non-empty, uniform-direction, never-null order naming both
    ///    `window_end` and `id` that its SPI promises — on this in-process
    ///    surface exactly as on REST, which is the point of doing it here
    ///    rather than in the handler. A first page has that order
    ///    *established* by [`establish_keyset_order`]; a continuation's
    ///    is *taken from its token* and then required to be one already —
    ///    see [`admit_continuation`] — because appending to it would widen
    ///    the sort tuple past the boundary values the token carries, and
    ///    honouring a caller order that disagreed with the token would
    ///    compare a row-value tuple against boundary values in another
    ///    order.
    /// 6. **Bind the cursor to its query.** [`read_fingerprint`] digests
    ///    the caller's `$filter` and all three typed parameters —
    ///    `gts_type_id`, the read range and `metadata_filter` — into the
    ///    value a continuation is bound to, and every dispatch carries it on
    ///    `filter_hash` so a conforming plugin mints it into
    ///    `next_cursor.f`. On a continuation, [`admit_continuation`]
    ///    refuses a token that carries a different one — or none. This is
    ///    the only place the property is
    ///    enforced: `filter_hash` is `None` for an in-process caller and
    ///    `toolkit_odata::validate_cursor_against` skips its comparison
    ///    when either side is `None`, so the REST edge deliberately passes
    ///    it `None` and keeps only its signed-token order check.
    /// 7. **Delegate** to the bound storage plugin's
    ///    `list_usage_records` SPI with the composed filter, the floored
    ///    order, the bound fingerprint, and the typed `time_range`, which
    ///    the plugin resolves as `from <= window_end < to`
    ///    (`cpt-cf-usage-collector-adr-window-end-selection`).
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::PermissionDenied`] /
    ///   [`UsageCollectorError::ServiceUnavailable`] when the PDP denies
    ///   or is unavailable, or when the PDP returns a constraint shape
    ///   this gear cannot honour (tree predicates on a flat resource,
    ///   unknown PEP property, type mismatch on a value).
    /// * [`UsageCollectorError::NotFound`] when `gts_type_id` does not
    ///   resolve to a declaration (never declared, or an incomplete
    ///   declaration) — new as of the declaration-resolution step above.
    /// * [`UsageCollectorError::InvalidArgument`] when `$filter` names a
    ///   reserved field, `metadata_filter` names an undeclared metadata
    ///   key, or the caller's order is not floorable into a sound keyset
    ///   (mixed sort directions, or a key that is not a mandatory record
    ///   attribute) — and, on a cursor request, when the order the token
    ///   was minted under is not one a conforming plugin could have
    ///   produced, or the token was minted over a different query than the
    ///   request carrying it — a different `$filter`, `gts_type_id`, range
    ///   or `metadata_filter`. A malformed range cannot
    ///   surface here: `time_range` arrives
    ///   already validated, because [`TimeRange`] has no public fields and
    ///   `TimeRange::new` is its only constructor.
    /// * Any other [`UsageCollectorError`] variant lifted from a plugin
    ///   transport / persistence failure.
    // @cpt-flow:cpt-cf-usage-collector-flow-usage-query-query-raw:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-fr-query-raw:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-fr-tenant-isolation:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-nfr-authorization:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-principle-pdp-centric-authorization:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-principle-fail-closed:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-constraint-no-business-logic:p1
    pub async fn list_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError> {
        let start = std::time::Instant::now();
        let result = async move {
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-pdp-delegate
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-attribution
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-pdp-deny-return
            let scope = authz::authorize_list_usage_records(
                &self.enforcer,
                self.metrics.as_ref(),
                PdpOp::QueryRaw,
                ctx,
            )
            .await
            .map_err(UsageCollectorError::from)?;
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-pdp-deny-return
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-attribution
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-pdp-delegate

            // Authorization composed → track in-flight for the remainder of
            // this attempt (decremented on every exit below, `?` included).
            let _inflight = QueryInflightGuard::enter(self.metrics.as_ref(), QueryKind::Raw);

            // Resolve the queried meter's declaration so the admissibility
            // gate below has a declared-keys set to check `metadata_filter`
            // against. Behavioural change: this path did not resolve a
            // declaration before Spec §3.11 gating landed here, so an
            // unresolvable type now fails closed as a pre-dispatch 404
            // where it previously reached the plugin regardless.
            let declaration = self.type_resolver.resolve(&gts_type_id).await?;

            // Gate the query surface on the resolved declaration (Spec
            // §3.11): `$filter` may not name a reserved field
            // (`gts_type_id`, the covered-period bounds — each already
            // travels as a typed parameter, so a predicate over one would
            // be a second, possibly contradictory, constraint), and
            // `metadata_filter` may not name a metadata key the
            // declaration does not declare, recomputed here per request so
            // a property declared a moment ago is usable on this very
            // call. `list_usage_records` takes no `group_by`, so
            // `require_dimensions_declared` does not apply on this path.
            if let Some(filter) = query.filter() {
                reject_reserved_filter_fields(filter)?;
            }
            require_metadata_filter_keys_declared(
                metadata_filter,
                declaration.metadata_schema.declared_keys(),
                &gts_type_id,
            )?;

            // The query a keyset continuation is bound to: the CALLER's
            // `$filter` plus every typed parameter that decides which rows
            // the page came from — the meter, the range and the metadata
            // filter, none of which is a `$filter` conjunct, so a filter
            // hash alone covers none of them. Computed before composition
            // AND-merges the server-injected PDP scope into `$filter`,
            // which is a value the next request's recomputation could
            // never reproduce (`compose_query_with_scope` documents the
            // same reasoning for why it preserves the caller's
            // `filter_hash`).
            //
            // It lives behind the service rather than at the REST edge
            // because there is one owner for the property on every
            // surface: `filter_hash` is `None` for an in-process caller
            // and `toolkit_odata::validate_cursor_against` skips its own
            // comparison when either side is `None`, so an edge-only check
            // would leave an in-process caller able to continue a cursor
            // minted under a different range and be served a silently
            // wrong page.
            let fingerprint = read_fingerprint(&gts_type_id, time_range, query, metadata_filter);

            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-constraint-composition
            let mut composed = compose_query_with_scope(query, &scope)?;
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-constraint-composition

            // The keyset floor: the SPI documents `query.order` as a
            // non-empty, uniform-direction, never-null keyset naming both
            // canonical fields, and this is the one place every caller
            // passes through — REST, the in-process client, and a direct
            // `Service` call alike. The branch is in the open because the
            // two modes are genuinely different operations: a first page's
            // order is normalized (and `establish_keyset_order` is
            // idempotent, so the REST handler having already done it makes
            // this a no-op), while a continuation's came from its token and
            // is only checked — refusing with a `400` on `cursor` rather
            // than being widened past the boundary values the token
            // carries. This is the only place a cursor-reconstructed order
            // is checked at all, since the handler skips the floor on that
            // path. Composition above only rewrites `filter`, so flooring
            // after it sees the order unchanged.
            match composed.cursor {
                // Bind, then check structure, then check relevance — see
                // `admit_continuation`. The fingerprint is passed in
                // rather than read back off `composed.filter_hash` after
                // the assignment below: reading it back would compare
                // against whichever of the two values happened to be
                // there, which before the assignment is the caller's own
                // filter-only hash (`compose_query_with_scope` preserves
                // it) and would refuse every legitimate page two.
                Some(_) => admit_continuation(&mut composed, &fingerprint)?,
                None => establish_keyset_order(&mut composed)?,
            }

            // Every dispatch carries the fingerprint — a continuation's
            // as much as a first page's, since a conforming plugin mints
            // `next_cursor.f` from whatever it was handed. Assigning it on
            // the first-page branch alone would leave page two dispatching
            // the caller's own filter-only hash, and page *three* would
            // then be refused for a mismatch nobody caused.
            composed.filter_hash = Some(fingerprint);

            let plugin = self
                .resolve_plugin_for(PluginOp::ListUsageRecords)
                .await
                .map_err(UsageCollectorError::from)?;

            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-plugin-dispatch
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-plugin-catch
            let page = instrument_spi(
                self.metrics.as_ref(),
                PluginOp::ListUsageRecords,
                plugin.list_usage_records(gts_type_id, time_range, &composed, metadata_filter),
            )
            .await
            .map_err(|e| UsageCollectorError::from(DomainError::from(e)))?;
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-plugin-catch
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-plugin-dispatch

            report_unbound_next_cursor(&page, composed.filter_hash.as_deref());
            Ok(page)
        }
        .await;
        let seconds = start.elapsed().as_secs_f64();
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-result-rows-observe
        if let Ok(page) = &result {
            self.metrics.observe_query_result_rows(
                QueryKind::Raw,
                u64::try_from(page.items.len()).unwrap_or(u64::MAX),
            );
        }
        // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-result-rows-observe
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-telemetry-complete
        let (outcome, error_category) = classify_query_result(&result);
        self.metrics
            .record_query_request(QueryKind::Raw, outcome, error_category, seconds);
        // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-telemetry-complete
        result
    }

    /// Aggregated read over `UsageRecord`s, narrowed by the PDP-returned
    /// constraints and executed server-side by the bound storage plugin.
    ///
    /// Mirrors [`Self::list_usage_records`] in posture. Five of that
    /// path's seven responsibilities live here per
    /// `cpt-cf-usage-collector-flow-usage-query-query-aggregated` — the
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
    ///    [`TypeResolver::resolve`], fail-closed. There is no caller-chosen
    ///    aggregation: the fold served is exactly the one the declaration
    ///    names, so no request can name a different one. Runs before any
    ///    plugin dispatch, so an unresolvable type never reaches the SPI.
    /// 3. **Gate** the query surface on that declaration (Spec §3.11), the
    ///    same three checks the raw path's step 3 runs — and unlike that
    ///    path, all three apply: `reject_reserved_filter_fields` on the
    ///    `$filter`, `require_dimensions_declared` on `group_by`, which
    ///    only this path takes, and `require_metadata_filter_keys_declared`
    ///    on the metadata side channel. Recomputed per request, so a
    ///    property declared a moment ago is usable on this very call.
    /// 4. **Compose** the PDP constraints into the user-supplied `OData`
    ///    filter via [`compose_query_with_scope`]. The composition is
    ///    intersection-only per
    ///    `cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2`.
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
    // @cpt-flow:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-usage-query-fr-query-aggregation:p1
    pub async fn query_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError> {
        let start = std::time::Instant::now();
        let result = async move {
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-pdp-delegate
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-attribution
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-pdp-deny-return
            let scope = authz::authorize_list_usage_records(
                &self.enforcer,
                self.metrics.as_ref(),
                PdpOp::QueryAggregated,
                ctx,
            )
            .await
            .map_err(UsageCollectorError::from)?;
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-pdp-deny-return
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-attribution
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-pdp-delegate

            // Authorization composed → track in-flight for the remainder of
            // this attempt (decremented on every exit below, `?` included).
            let _inflight = QueryInflightGuard::enter(self.metrics.as_ref(), QueryKind::Aggregated);

            // Resolve the queried meter's declaration before any plugin
            // dispatch: a meter declares exactly one fold, and this gear
            // serves that fold and no other — there is no request-supplied
            // aggregation to validate against a kind. An unresolvable type
            // (never declared, or an incomplete declaration) fails closed
            // here as a pre-dispatch 404, so the plugin stays pure
            // persistence and never sees a type it cannot resolve.
            let declaration = self.type_resolver.resolve(&gts_type_id).await?;

            // Gate the query surface on the resolved declaration (Spec
            // §3.11): `$filter` may not name a reserved field
            // (`gts_type_id`, the covered-period bounds), and `group_by` /
            // `metadata_filter` may not name a metadata key the
            // declaration does not declare — three checks over two
            // disjoint channels, since `metadata_filter` is the
            // dynamic-key side channel `$filter` cannot express a JSON-map
            // key predicate through. All three are recomputed here per
            // request so a property declared a moment ago is usable on
            // this very call. Runs after resolving the declaration (there
            // is nothing to check `group_by` / `metadata_filter` against
            // before then) and before composing the PDP scope.
            if let Some(filter) = query.filter() {
                reject_reserved_filter_fields(filter)?;
            }
            require_dimensions_declared(
                group_by,
                declaration.metadata_schema.declared_keys(),
                &gts_type_id,
            )?;
            require_metadata_filter_keys_declared(
                metadata_filter,
                declaration.metadata_schema.declared_keys(),
                &gts_type_id,
            )?;

            let plugin = self
                .resolve_plugin_for(PluginOp::QueryAggregatedUsageRecords)
                .await
                .map_err(UsageCollectorError::from)?;

            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-constraint-composition
            let composed = compose_query_with_scope(query, &scope)?;
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-constraint-composition

            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-plugin-dispatch
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-plugin-catch
            // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-plugin-catch-return
            instrument_spi(
                self.metrics.as_ref(),
                PluginOp::QueryAggregatedUsageRecords,
                plugin.query_aggregated_usage_records(
                    gts_type_id,
                    time_range,
                    declaration.aggregation_fold,
                    &composed,
                    metadata_filter,
                    group_by,
                ),
            )
            .await
            .map_err(|e| UsageCollectorError::from(DomainError::from(e)))
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-plugin-catch-return
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-plugin-catch
            // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-plugin-dispatch
        }
        .await;
        // Enforce the declared aggregate-bucket cap (`AggregationResult.buckets`
        // `maxItems` in `usage-collector-v1.yaml`).
        // The plugin bounds its own scan to `MAX_AGGREGATION_BUCKETS + 1` rows
        // (its memory guard), so an over-cap result surfaces here as strictly
        // more than the cap — reject it as the client-fixable 400 it is rather
        // than returning an oversized page. Runs before the result-row telemetry
        // below so the rejection is classified as a client error, not a page.
        let result = result.and_then(|aggregation_result| {
            if aggregation_result.buckets.len() > MAX_AGGREGATION_BUCKETS {
                Err(UsageCollectorError::aggregation_result_too_large(
                    MAX_AGGREGATION_BUCKETS,
                ))
            } else {
                Ok(aggregation_result)
            }
        });
        let seconds = start.elapsed().as_secs_f64();
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-result-rows-observe
        if let Ok(aggregation_result) = &result {
            self.metrics.observe_query_result_rows(
                QueryKind::Aggregated,
                u64::try_from(aggregation_result.buckets.len()).unwrap_or(u64::MAX),
            );
        }
        // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-result-rows-observe
        // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-telemetry-complete
        let (outcome, error_category) = classify_query_result(&result);
        self.metrics
            .record_query_request(QueryKind::Aggregated, outcome, error_category, seconds);
        // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-telemetry-complete
        result
    }

    /// Lazily resolves and returns the bound storage-plugin client.
    ///
    /// On the first dispatch the embedded `GtsPluginSelector` resolves the
    /// instance id single-flight via [`Self::resolve_plugin`] and caches it for
    /// the `Service`'s lifetime; warm calls reuse the cached id with no further
    /// `types-registry` round-trip. The resolved scoped client is looked up via
    /// `ClientHub::try_get_scoped`; the structural readiness fact (selector
    /// cached AND the scoped client registered) governs whether the dispatch
    /// proceeds.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::TypesRegistryUnavailable`] when the registry call
    /// fails (the selector stays uncached so the next dispatch retries),
    /// [`DomainError::PluginNotFound`] when no instance matches the configured
    /// vendor, [`DomainError::InvalidPluginInstance`] on malformed instance
    /// content, and [`DomainError::PluginUnavailable`] when the scoped client is
    /// not registered under the resolved scope.
    // @cpt-flow:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-fr-pluggable-storage:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-entity-plugin-binding:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-nfr-availability:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-principle-pluggable-storage:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-principle-plugin-resolution-via-client-hub:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-principle-fail-closed:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-adr-pluggable-storage:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-constraint-vendor-pluggable:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-constraint-plugin-contract-stability:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-constraint-nfr-thresholds:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-contract-storage-plugin:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-contract-gts-registry:p1
    pub async fn get_plugin(&self) -> Result<Arc<dyn UsageCollectorPluginV1>, DomainError> {
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-enter-selector
        // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-cold-path
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-cold-path
        // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-cache-instance-id
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-cache-instance-id
        // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-warm-path
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-warm-path
        let instance_id = match self.selector.get_or_init(|| self.resolve_plugin()).await {
            Ok(instance_id) => instance_id,
            // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-warm-path
            // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-warm-path
            // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-cache-instance-id
            // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-cache-instance-id
            // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-catch
            // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-plugin-unavailable-cold
            Err(e) => {
                // Selector resolution failed — structural readiness fact does
                // not hold.
                // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-readiness-fact
                self.metrics.set_plugin_ready(false);
                // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-readiness-fact
                tracing::warn!(
                    error = %e,
                    vendor = %self.vendor,
                    "usage-collector plugin selector resolution failed"
                );
                return Err(e);
            } // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-plugin-unavailable-cold
              // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-catch
        };
        // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-cold-path
        // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-cold-path
        // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-enter-selector

        // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-try-get-scoped
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-try-get-scoped
        let scope = ClientScope::gts_id(instance_id.as_ref());
        let client = self
            .hub
            .try_get_scoped::<dyn UsageCollectorPluginV1>(&scope);
        // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-try-get-scoped
        // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-try-get-scoped

        // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-return-handle
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-return
        if let Some(client) = client {
            // Structural readiness fact holds: selector cached an instance id
            // AND the scoped client is registered.
            // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-readiness-fact
            // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-readiness-fact
            self.metrics.set_plugin_ready(true);
            // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-readiness-fact
            // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-readiness-fact
            return Ok(client);
        }

        // Scoped client not registered — structural readiness fact does not hold.
        // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-readiness-fact
        // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-readiness-fact
        self.metrics.set_plugin_ready(false);
        // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-readiness-fact
        // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-readiness-fact
        tracing::warn!(
            plugin_gts_id = %instance_id,
            vendor = %self.vendor,
            "usage-collector storage plugin client not registered yet"
        );
        Err(DomainError::PluginUnavailable {
            gts_id: Some(instance_id.to_string()),
            reason: "client not registered yet".into(),
        })
        // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-return
        // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-return-handle
    }

    /// Resolve the bound storage-plugin handle for an SPI dispatch, recording
    /// `uc_plugin_accept_errors_total{operation, error_category="unready"}`
    /// when the structural binding is unavailable (no SPI dispatch occurred,
    /// so no duration is recorded). `op` labels the SPI method that would have
    /// been dispatched; when a method issues several SPI calls, the first is
    /// used (they share one handle, so a resolution failure aborts them all).
    /// The `uc_plugin_ready` gauge is maintained by [`Self::get_plugin`].
    // @cpt-algo:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1
    // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-resolve
    async fn resolve_plugin_for(
        &self,
        op: PluginOp,
    ) -> Result<Arc<dyn UsageCollectorPluginV1>, DomainError> {
        match self.get_plugin().await {
            Ok(plugin) => Ok(plugin),
            // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-unready
            // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-unready-counter
            Err(e) => {
                self.metrics
                    .record_plugin_accept_error(op, PluginErrorCategory::Unready);
                Err(e)
            } // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-unready-counter
              // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-unready
        }
    }
    // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-spi-dispatch-instrumentation:p1:inst-algo-plugin-dispatch-resolve

    /// Resolves the bound storage-plugin instance id from `types-registry`.
    // @cpt-begin:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-lazy-resolve
    // @cpt-begin:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-resolve-plugin
    #[tracing::instrument(skip_all, fields(vendor = %self.vendor))]
    async fn resolve_plugin(&self) -> Result<String, DomainError> {
        info!("Resolving usage-collector storage plugin");

        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        let plugin_type_id = UsageCollectorPluginSpecV1::gts_type_id().clone();

        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{plugin_type_id}*")))
            .await
            .map_err(TypesRegistryError::from)?;

        let gts_id = choose_plugin_instance::<UsageCollectorPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )?;

        info!(plugin_gts_id = %gts_id, "Selected usage-collector storage plugin instance");

        Ok(gts_id)
    }
    // @cpt-end:cpt-cf-usage-collector-algo-foundation-plugin-host-binding:p2:inst-algo-binding-resolve-plugin
    // @cpt-end:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1:inst-binding-lazy-resolve

    /// Test-only: clear the cached binding so the next dispatch re-resolves.
    #[cfg(test)]
    pub(crate) async fn selector_reset_for_test(&self) -> bool {
        self.selector.reset().await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_tests.rs"]
mod service_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_metrics_tests.rs"]
mod service_metrics_tests;
