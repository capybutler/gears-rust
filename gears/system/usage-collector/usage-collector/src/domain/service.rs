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

use std::collections::hash_map::Entry;
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
use toolkit_odata::{CursorV1, ODataQuery, Page as ODataPage, PageInfo, ast};
use toolkit_security::{AccessScope, SecurityContext};
use tracing::{Instrument, info};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient, TypesRegistryError};
use usage_collector_sdk::{
    AggregationDimension, AggregationResult, ConflictOutcome, CreateUsageRecord, CursorField,
    EntryType, FeedPage, FeedPosition, FeedStart, FeedSubscription, IngestionQuotaExceededArgs,
    Invalidation, MAX_AGGREGATION_BUCKETS, MetadataFilter, MeterRef, MeterTypeId, NotFoundReason,
    ReconciliationMetadata, RecordOrigin, StoredUsageRecord, TimeRange, UsageCollectorError,
    UsageCollectorPluginError, UsageCollectorPluginSpecV1, UsageCollectorPluginV1, UsageRecord,
    ValidationReason,
};
use uuid::Uuid;

use crate::config::IngestionQuotaConfig;
use crate::domain::authz::{self, AttributionTupleKey};
use crate::domain::covered_period::{
    CoveredPeriodBounds, enforce_covered_period_bounds, ingestion_action,
};
use crate::domain::feed;
use crate::domain::invalidation::{derive_invalidation_target, verify_invalidation_target};
use crate::domain::meter_reverse::MeterReverseResolver;
use crate::domain::observability::{OperationLogEntry, log_operation_completed};
use crate::domain::ports::declaration_mirror::NoopDeclarationMirror;
use crate::domain::ports::declarations::UnavailableDeclarationSource;
use crate::domain::ports::metrics::{
    FeedErrorCategory, IngestRequestErrorCategory, IngestRequestOutcome, NoopMetrics, PdpOp,
    PluginErrorCategory, PluginOp, QueryErrorCategory, QueryKind, RecordErrorCategory,
    RecordOutcome, RequestOutcome, UsageCollectorMetrics,
};
use crate::domain::query;
use crate::domain::query::{
    admit_continuation, compose_query_with_scope, establish_keyset_order, read_fingerprint,
    reject_off_label_literals, reject_unpublished_filter_fields, require_dimensions_declared,
    require_metadata_filter_keys_declared, require_metadata_filter_within_caps,
};
use crate::domain::quota::IngestionQuota;
use crate::domain::reconciliation;
use crate::domain::type_resolver::{ResolvedDeclaration, TypeResolver, TypeResolverConfig};
use crate::domain::validation::{DEFAULT_METADATA_SIZE_CAP_BYTES, validate_submit_record_metadata};

use super::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_dispatch_error,
    lift_domain_error,
};

/// Default per-request entry cap, used when `[usage_collector].max_batch_records`
/// is not configured. The cap is operator configuration (DESIGN §3.2), so the
/// wire schema publishes no `maxItems`; it bounds what one caller sends, not
/// what one backend write holds.
pub const DEFAULT_MAX_BATCH_RECORDS: usize = 100;

/// Default `[usage_collector].unavailable_retry_after_secs`, used when the
/// key is not configured. DESIGN §3.8 publishes `1` for this key: a
/// `ServiceUnavailable` that reached no retry hint of its own (a hintless
/// plugin `Transient`, or a dispatch that reached no plugin at all) carries
/// this delay rather than none.
pub const DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS: u64 = 1;

/// Default `[usage_collector].target_not_converged_retry_after_secs`, used
/// when the key is not configured. DESIGN §3.8 publishes `1` for this key: a
/// `Conflict(TargetNotConverged)` carries this delay.
pub const DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS: u64 = 1;

/// Concurrency cap for the per-distinct-attribution-tuple PDP fan-out in
/// `create_usage_records`. Sized to match the platform's established
/// external-call posture (8) so a worst-case all-distinct cap-sized batch
/// takes `ceil(cap / 8) × PDP_RTT` wall-clock without overwhelming the PDP
/// transport pool. Bounds the `inst-auth-ingest-pdp` step of
/// `cpt-cf-usage-collector-flow-authorize-ingestion`.
const PDP_CONCURRENCY: usize = 8;

/// The `PageInfo.limit` reported on a raw-path page when `query.limit` is
/// absent.
///
/// REST's `prepare_list_query` always populates `query.limit` before
/// dispatch, so this default is reached only by a bare in-process caller.
/// Mirrors the `TimescaleDB` plugin's own private `DEFAULT_PAGE_SIZE`
/// (`plugins/timescaledb-usage-collector-plugin/src/infra/storage/record_store.rs`)
/// so the reported figure matches what that backend paginates by when it is
/// not told otherwise.
///
/// Always used together with [`MAX_PAGE_SIZE`] as a clamp, never alone: an
/// in-process caller's `limit` does not pass REST's `$top` cap, so
/// `composed.limit.unwrap_or(DEFAULT_PAGE_SIZE)` alone would report a
/// `PageInfo.limit` of `5000` (or `0`) for a page every plugin still serves
/// at most [`MAX_PAGE_SIZE`] rows of. `local_client.rs` forwards the page
/// verbatim, so a wrong figure here reaches a calling gear as fact.
const DEFAULT_PAGE_SIZE: u64 = 100;

/// Maximum number of records a raw-path page may report as its
/// `PageInfo.limit`, and the REST `$top` / `limit` ceiling a caller-supplied
/// value above it is rejected against
/// (`cpt-cf-usage-collector-constraint-nfr-thresholds`).
///
/// Domain-owned, on [`crate::domain::feed::MAX_FEED_LIMIT`]'s precedent: the
/// REST handler (`api::rest::handlers::usage_records`) re-exports this
/// rather than defining its own, so the wire-edge cap and the domain-level
/// clamp below are one constant rather than two that could drift apart
/// across the module boundary.
pub(crate) const MAX_PAGE_SIZE: u64 = 1000;

/// Concurrency cap for the per-distinct-`gts_type_id` Type Resolver fan-out in
/// `create_usage_records`. Bounds request-local pressure on
/// [`TypeResolver::resolve`] (itself single-flighted per key, and normally
/// served from cache — see [`crate::domain::type_resolver`]) for the
/// type-resolution pre-pass; sized identically to [`PDP_CONCURRENCY`] (the
/// three request-local fan-outs — this one, PDP above and the
/// invalidation-target one below — run sequentially, not concurrently, so
/// the effective in-flight ceiling stays at 8).
const TYPE_RESOLUTION_FANOUT_CONCURRENCY: usize = 8;

/// Concurrency cap for the per-distinct-target `get_usage_record` fan-out
/// that resolves a batch's invalidation references in
/// `create_usage_records`. Bounds plugin-side pressure for the target
/// pre-check, and is 8 for the platform's established external-call posture
/// — stated rather than delegated to the two caps above, so the value
/// survives either of them changing. Bounds the
/// `inst-algo-semantics-l1-bounded-fanout` step of
/// `cpt-cf-usage-collector-algo-target-resolution`.
///
/// **A deliberate default, not a pinned one**, as are the two caps above:
/// no test constrains the number, because pinning it would take a
/// timing-dependent test that observes concurrency.
const TARGET_LOOKUP_FANOUT_CONCURRENCY: usize = 8;

const PLUGIN_SPI_DISPATCH_TIMEOUT: Duration = Duration::from_millis(250);

/// One PDP fan-out outcome: the input indices that share an attribution
/// tuple plus the `Result<AccessScope, DomainError>` returned for that
/// tuple's representative call. Decision projection (success / deny /
/// unavailable → per-index `results[index]` slot) reads this shape; a
/// permitted group's granted [`AccessScope`] is kept so the group's
/// invalidation entries (if any) can compile it into their target lookup
/// scope.
type PdpGroupDecision = (Vec<usize>, Result<AccessScope, DomainError>);

/// Per-input-index slot [`project_pdp_decisions`] fills for a submission
/// carrying a withdrawal: `None` when the entry's PDP group never reached
/// scope compilation (an ordinary measurement, or an index the loop never
/// visited), `Some(Ok((scope_id, compiled)))` once its group's permit
/// compiled, `Some(Err(_))` when compilation itself failed.
type LookupScopeSlot = Option<Result<(usize, Arc<ast::Expr>), DomainError>>;

/// Cached resolution per distinct meter, lifted into [`DomainError`] so one
/// resolution outcome projects to every record sharing that type without
/// re-resolving it.
type DeclarationCache = HashMap<MeterTypeId, Result<Arc<ResolvedDeclaration>, DomainError>>;

/// Cached target lookup per distinct `(derived target, lookup scope id)`,
/// lifted into [`DomainError`] so the variant identity of
/// `UsageRecordNotFound { id }` survives the cache. The scope id keys the
/// read to the permit that authorized it, so two entries naming the same
/// target under different permits are not served from one another's read.
type InvalidationTargetCache = HashMap<(Uuid, usize), Result<StoredUsageRecord, DomainError>>;

/// One entry whose target check was deferred to the post-loop pre-pass.
/// Entries reach it only after passing PDP and the declaration-resolution
/// pre-pass, and only when they declare `entry_type: invalidation`.
///
/// A named struct rather than a tuple, and a correctness choice: two of its
/// four record-shaped members and the pairing itself (input index, fan-out
/// key) are where a positional alignment slip would hide, and a slip in
/// either direction rejects the wrong submission with someone else's
/// identifier.
struct PendingInvalidationTarget {
    /// Input index of the submission, and the only slot in `results` this
    /// entry's outcome may be projected into.
    index: usize,
    /// The submission as the caller sent it. The faithful-copy comparator
    /// runs against this rather than against `record`: the submission has
    /// no identity yet, which is the honest reason `id` is not compared,
    /// and destructuring the *submission* shape is what makes a field added
    /// to [`CreateUsageRecord`] alone a compile error there.
    submission: CreateUsageRecord,
    /// The withdrawal the projected `record` carries: the reason the
    /// submission stated, paired with the target the gateway derived from
    /// the submission's own identity inputs. Unwrapped once here so the
    /// verification cannot be reached for an entry that has none. Its
    /// `target` is the fan-out key and the identifier the rejection echoes.
    invalidation: Invalidation,
    /// The meter this entry resolved to, taken off the declaration the
    /// pre-pass already fetched. Carried here rather than looked up again so
    /// the reference the faithful-copy comparison reads and the reference
    /// the dispatch is keyed on are one value.
    meter: MeterRef,
    /// The projected entry, dispatched to the plugin once verified.
    record: UsageRecord,
    /// The compiled scope of the `create` permit that authorized this
    /// submission. The target is read under it.
    lookup_scope: Arc<ast::Expr>,
    /// Identifies [`Self::lookup_scope`] within the request: one id per PDP
    /// tuple group, so entries sharing a permit share a lookup.
    lookup_scope_id: usize,
}

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
/// would otherwise be relabelled and served as fact. [`reattach`] is the same
/// check where a bare `Result` suffices; this one needs a [`Resolution`]
/// beside it, so the comparison is spelled out rather than called.
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
        let detail = foreign_reference_detail(stored_type_uuid, meter);
        (
            Err(invariant_breach(detail.clone())),
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

/// The detail a record answering under another meter's reference is refused
/// with.
///
/// One spelling for [`reattach`] and for the conflict arm of
/// [`settle_dispatched`], which reaches the same breach by a different path
/// and has to report it as a [`DomainError`] as well as to the caller.
///
/// It names the reference the gear asked for and the one it got. Both are
/// internal values the caller never supplied and neither names a GTS type,
/// so neither leaks a type identity across a tenant boundary.
fn foreign_reference_detail(stored_type_uuid: Uuid, meter: &MeterRef) -> String {
    format!(
        "storage plugin answered under registry reference {stored_type_uuid} for a query on {}",
        meter.uuid,
    )
}

/// Re-attach `meter`'s identifier to a record the plugin returned.
///
/// The reference is checked first. Re-attachment staples an identifier the
/// *gear* holds onto a record the *plugin* produced, so a row answering
/// under another meter's reference would otherwise be relabelled with the
/// queried meter's identifier and served to a caller as fact. That is the
/// same class of store breach as a `get_usage_record` answering one `id`
/// with another row, and it is refused the same way: a typed `Internal`,
/// never a panic, on a request path.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn reattach(
    stored: StoredUsageRecord,
    meter: &MeterRef,
) -> Result<UsageRecord, UsageCollectorError> {
    if stored.gts_type_uuid != meter.uuid {
        return Err(invariant_breach(foreign_reference_detail(
            stored.gts_type_uuid,
            meter,
        )));
    }
    Ok(stored.into_usage_record(meter.id.clone()))
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
/// them. Host-side dispatch deadline expiry is recorded by `instrument_spi`
/// as `timeout`, before this classifier is consulted.
fn backend_error_category(err: &UsageCollectorPluginError) -> Option<PluginErrorCategory> {
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
async fn instrument_spi<T>(
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
        match tokio::time::timeout(PLUGIN_SPI_DISPATCH_TIMEOUT, fut.instrument(span)).await {
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
    // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-inflight-increment
    // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-inflight-increment
    fn enter(metrics: &'a dyn UsageCollectorMetrics, kind: QueryKind) -> Self {
        metrics.query_inflight_inc(kind);
        Self { metrics, kind }
    }
    // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-inflight-increment
    // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-inflight-increment
}

impl Drop for QueryInflightGuard<'_> {
    fn drop(&mut self) {
        self.metrics.query_inflight_dec(self.kind);
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
/// [`crate::domain::query::require_cursor_fingerprint`], so this seam is the
/// only place that category can arise. Which category a cursor rejection
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
fn classify_query_result<T>(
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
fn classify_feed_result<T>(
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
fn report_absent_live_continuation(
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
fn lift_feed_dispatch_error(
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

/// Collapse a PDP denial into `NotFound` so the by-id point lookup
/// (`get`) never acts as an existence oracle; every other error (notably
/// `ServiceUnavailable`, which leaks nothing) is preserved. That lookup
/// is the gear's only by-id surface — a withdrawal is an ordinary
/// ingested entry on the create path, not a second lookup-then-mutate
/// operation — so this has one caller.
///
/// Reusing `usage_record_not_found` whole, `NotFoundReason` included, is the
/// invariant and not an implementation detail: the collapsed denial must be
/// byte-identical to a genuine miss on every channel a caller can read, and
/// `NotFoundReason` is one of those channels for an in-process consumer.
/// Minting a distinct reason for the denial would restore the oracle.
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

/// Project the batch PDP fan-out's per-group decisions from
/// [`Service::create_usage_records_inner`] onto `results` (a denied /
/// unavailable group) and a per-input-index [`LookupScopeSlot`] (a permitted
/// group), compiling each permitted group's [`AccessScope`] into an
/// `OData` filter **once** and sharing the compiled scope across every
/// invalidation entry the group covers — every entry in a group already
/// shares one PDP decision, so it can share one compiled scope too. A group
/// that authorizes no invalidation skips compilation entirely: an ordinary
/// measurement never triggers a target lookup.
///
/// Extracted from the host body (alongside [`resolve_invalidation_targets`])
/// to keep `create_usage_records_inner` under the cognitive-complexity and
/// line-count caps.
fn project_pdp_decisions(
    pdp_decisions: Vec<PdpGroupDecision>,
    withdrawals: &[Option<CreateUsageRecord>],
    submission_count: usize,
    results: &mut [Option<Result<UsageRecord, UsageCollectorError>>],
    pdp_allowed: &mut [bool],
) -> Vec<LookupScopeSlot> {
    let mut lookup_scopes: Vec<LookupScopeSlot> = (0..submission_count).map(|_| None).collect();
    let mut next_scope_id = 0_usize;
    for (indices, decision) in pdp_decisions {
        match decision {
            Ok(scope) => {
                if indices.iter().any(|index| withdrawals[*index].is_some()) {
                    let scope_id = next_scope_id;
                    next_scope_id += 1;
                    let compiled =
                        authz::scope_to_odata_filter(&scope).map(|expr| (scope_id, Arc::new(expr)));
                    for index in indices {
                        lookup_scopes[index] = Some(compiled.clone());
                    }
                }
            }
            Err(e) => {
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
    }
    lookup_scopes
}

/// Resolve every deferred invalidation-target check from
/// [`Service::create_usage_records`]'s validation loop.
///
/// Builds a request-local `Map<(target, permit scope), Result<UsageRecord,
/// _>>` via a bounded `get_usage_record` fan-out over the **distinct**
/// `(target, lookup scope id)` pairs — a batch withdrawing one target twice
/// under one permit costs one read, not two (`inst-algo-semantics-l1-dedup` /
/// `inst-algo-semantics-l1-bounded-fanout`) — then, for every entry in
/// `pending`, runs [`verify_invalidation_target`] and the deferred metadata
/// check, projecting the outcome into `results` (rejection) or `eligible`
/// (verified).
///
/// The metadata check stays behind the target check so a submission breaking
/// both is told about the copy: the metadata it would be told to fix is
/// metadata it has to copy from the target regardless.
///
/// **Pairing a row to its entry is this function's obligation, in both
/// directions**, since [`verify_invalidation_target`] takes the row it is
/// handed and never re-checks that it is the row the entry named.
///
/// *Request-local:* the fan-out key and the `results` slot are read off one
/// destructured [`PendingInvalidationTarget`], so no entry is verified against
/// a row fetched for a different entry and no outcome lands on a different
/// input index.
///
/// *Store-side:* the returned row's `id` must equal the id it was fetched for.
/// This cannot be left to the SPI contract: a submission that happens to be a
/// faithful copy of the *returned* row would be **accepted**, withdrawing an
/// entry nothing ever checked.
///
/// At-most-one-invalidation is not checked here: every withdrawal of one record
/// reaches the same six identity inputs, so a second collides on the dedup
/// identity at the store and is lifted at dispatch ([`lift_dispatch_error`]).
///
/// **Traceability.** The entry-type guard below is
/// `cpt-cf-usage-collector-dod-no-invalidation-of-invalidation`'s second half
/// on the batch path: a plugin returning an invalidation under that identifier
/// is a contract breach and surfaces as an internal failure rather than a
/// caller error. Its first half holds by construction, through
/// [`derive_invalidation_target`]'s fixed `entry_type = record`.
// @cpt-algo:cpt-cf-usage-collector-algo-target-resolution:p1
// @cpt-dod:cpt-cf-usage-collector-dod-no-invalidation-of-invalidation:p1
// @cpt-begin:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-dedup
// @cpt-begin:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-bounded-fanout
// @cpt-begin:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-lookup
#[allow(clippy::too_many_arguments)] // the pre-pass reads the request's caches and writes its result slots; each argument is one of those
async fn resolve_invalidation_targets(
    plugin: &dyn UsageCollectorPluginV1,
    metrics: &dyn UsageCollectorMetrics,
    pending: Vec<PendingInvalidationTarget>,
    declaration_cache: &DeclarationCache,
    metadata_size_cap_bytes: usize,
    results: &mut [Option<Result<UsageRecord, UsageCollectorError>>],
    eligible: &mut Vec<(usize, MeterRef, UsageRecord)>,
    batch_ids: &HashSet<Uuid>,
    unavailable_retry_after_secs: u64,
    target_not_converged_retry_after_secs: u64,
) {
    if pending.is_empty() {
        return;
    }

    let distinct_lookups: HashMap<(Uuid, usize), Arc<ast::Expr>> = pending
        .iter()
        .map(|entry| {
            (
                (entry.invalidation.target, entry.lookup_scope_id),
                Arc::clone(&entry.lookup_scope),
            )
        })
        .collect();

    let target_cache: InvalidationTargetCache =
        stream::iter(distinct_lookups.into_iter().map(|(key, scope)| async move {
            let (target, _) = key;
            let outcome = instrument_spi(
                metrics,
                PluginOp::GetUsageRecord,
                plugin.get_usage_record(target, &scope, true),
            )
            .await
            .map_err(|e| match e {
                UsageCollectorPluginError::UsageRecordNotConverged { .. } => {
                    DomainError::TargetNotConverged { target }
                }
                other => DomainError::from(other),
            });
            (key, outcome)
        }))
        .buffer_unordered(TARGET_LOOKUP_FANOUT_CONCURRENCY)
        .collect()
        .await;

    for entry in pending {
        let PendingInvalidationTarget {
            index,
            submission,
            invalidation,
            meter,
            record,
            lookup_scope: _,
            lookup_scope_id,
        } = entry;

        // @cpt-begin:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-not-found
        // The pre-pass populates the cache for every pending target, so a
        // missing entry here is a host-invariant breach: a typed `Internal`
        // per-record error, never `unreachable!()` — request paths must not
        // panic on an invariant failure.
        let target = match target_cache.get(&(invalidation.target, lookup_scope_id)) {
            Some(Ok(row)) => row,
            Some(Err(DomainError::UsageRecordNotFound { .. })) => {
                results[index] = Some(Err(if batch_ids.contains(&invalidation.target) {
                    // Not a realization site for `inst-err-not-converged`: the
                    // plugin answered `UsageRecordNotFound`, and this is the
                    // host's own same-batch inference reusing the conflict
                    // shape and its configured delay.
                    UsageCollectorError::target_not_converged(
                        invalidation.target,
                        Some(target_not_converged_retry_after_secs),
                    )
                } else {
                    UsageCollectorError::invalidation_target_not_found(invalidation.target)
                }));
                continue;
            }
            Some(Err(e)) => {
                results[index] = Some(Err(lift_domain_error(
                    e.clone(),
                    UnavailableRetryAfterSecs(unavailable_retry_after_secs),
                    TargetNotConvergedRetryAfterSecs(target_not_converged_retry_after_secs),
                )));
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
        // @cpt-end:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-not-found

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

        // A row answering an identifier derived with `entry_type = record`
        // cannot be an invalidation; one that is contradicts its own
        // identifier. Same class of store breach as the id mismatch above,
        // refused rather than accepted as a withdrawal of a withdrawal.
        if target.entry_type() == EntryType::Invalidation {
            results[index] = Some(Err(invariant_breach(format!(
                "storage plugin answered get_usage_record({}) with an invalidation",
                invalidation.target,
            ))));
            continue;
        }

        // The comparator is handed the submission, not `record`: the two
        // carry the same caller-supplied fields, but only the submission
        // shape makes a field added to it alone a compile error there.
        if let Err(e) = verify_invalidation_target(&submission, meter.uuid, &invalidation, target) {
            results[index] = Some(Err(e));
            continue;
        }

        // Metadata check deferred behind the target check, per the
        // error-priority ordering. A missing declaration entry here is a
        // host-invariant breach (the pre-pass covers every PDP-allowed
        // record's gts_type_id); typed `Internal`, never a panic.
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

        eligible.push((index, meter, record));
    }
}
// @cpt-end:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-lookup
// @cpt-end:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-bounded-fanout
// @cpt-end:cpt-cf-usage-collector-algo-target-resolution:p1:inst-algo-semantics-l1-dedup

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
async fn dispatch_eligible_entries(
    plugin: &dyn UsageCollectorPluginV1,
    metrics: &dyn UsageCollectorMetrics,
    eligible: Vec<(usize, MeterRef, UsageRecord)>,
    results: &mut [Option<Result<UsageRecord, UsageCollectorError>>],
    unavailable_retry_after_secs: u64,
    target_not_converged_retry_after_secs: u64,
) -> Result<(), UsageCollectorError> {
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
    let spi_results = instrument_spi(
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
        return Err(invariant_breach(format!(
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
                None => Err(invariant_breach(format!(
                    "no dispatched entry resolved identity {} of input {index}",
                    record.id
                ))),
            },
        );
    }

    Ok(())
}

/// `usage-collector` domain service.
///
/// Discovers the bound storage plugin via `types-registry` and delegates **the
/// entry ledger** to it. Owns the lazy binding resolution.
///
/// **Not all durable state goes through the SPI.** The Type Resolver owns
/// DESIGN §3.7's declaration mirror — the one durable table this gear has of
/// its own, reached over `toolkit-db` rather than through the Plugin SPI
/// (`cpt-cf-usage-collector-adr-declaration-rehydration`, whose first
/// Consequence retires the all-durable-state-behind-the-SPI reading of
/// `cpt-cf-usage-collector-topology-gear-runtime`). The entry ledger — every
/// `usage_records` row — is wholly the plugin's, and that is what this type
/// delegates.
///
/// `Service` is the single Ingestion Gateway component
/// (`cpt-cf-usage-collector-dod-ingestion-choke-point`): every submission —
/// REST or SDK, live or backfill, record or invalidation — enters through
/// one of its public `create_usage_records*` / `backfill_usage_records*`
/// methods, all of which funnel into `create_usage_records_inner`, which
/// dispatches to the storage plugin and nowhere else; there is no second,
/// parallel write path.
// @cpt-state:cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle:p2
// @cpt-dod:cpt-cf-usage-collector-dod-ingestion-choke-point:p1
#[domain_model]
pub struct Service {
    hub: Arc<ClientHub>,

    /// Vendor selector read once at `Gear::init`; changing it requires a
    /// gear restart.
    vendor: String,

    // @cpt-dod:cpt-cf-usage-collector-component-plugin-host:p2
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
    /// and by the ingestion path ([`Self::create_usage_records_inner`]) to
    /// validate a submission's metadata against the declared closed surface — see
    /// [`crate::domain::type_resolver`].
    type_resolver: Arc<TypeResolver>,

    /// The forward resolver's counterpart: a registry reference back to the
    /// meter identifier it was issued for.
    ///
    /// Consulted by [`Self::get_usage_record`] and by nothing else: that
    /// method takes no meter, so the entry a plugin answers it with names its
    /// meter by reference alone and there is no identifier in scope to
    /// re-attach. Built over the same adapter and mirror as `type_resolver`
    /// ([`crate::infra::types_registry_source::build_default_resolvers`]), so
    /// a declaration resolved one way populates the cache the other reads.
    reverse_resolver: Arc<MeterReverseResolver>,

    /// Cap on an entry's serialized metadata map, in bytes, enforced by
    /// [`validate_submit_record_metadata`] on every ingestion path.
    /// [`Self::new_with_metrics`] takes it as a mandatory `usize`;
    /// [`Self::new`] is the one place [`DEFAULT_METADATA_SIZE_CAP_BYTES`]
    /// applies, production bootstrap threading the configured value instead.
    metadata_size_cap_bytes: usize,

    /// The covered-period bounds both ingestion paths enforce, projected
    /// from the configured `[usage_collector]` block by
    /// `UsageCollectorConfig::covered_period_bounds`. Held as the finished
    /// [`CoveredPeriodBounds`] rather than as the whole config, for the
    /// same reason `metadata_size_cap_bytes` is held as a plain `usize`.
    covered_period_bounds: CoveredPeriodBounds,

    /// The per-request entry cap both batch ingestion routes enforce
    /// (`create_usage_records_for_origin`). Defaults to
    /// [`DEFAULT_MAX_BATCH_RECORDS`] via [`Self::new`]; production bootstrap
    /// (`module.rs`) instead threads `UsageCollectorConfig::max_batch_records`
    /// through explicitly, mirroring `metadata_size_cap_bytes`.
    max_batch_records: usize,

    /// The per-subject ingestion token buckets the ingestion path charges
    /// (DESIGN §3.2, Ingestion Admission Control).
    ///
    /// **One** set of buckets for the whole service, deliberately: nothing
    /// here is keyed on route, origin or path. A second bucket per route is
    /// what `cpt-cf-usage-collector-adr-backfill-isolation` rules out —
    /// "Backfill draws the same allowance as live emission. Workload
    /// isolation, not a separate budget, is what distinguishes the routes."
    ///
    /// Built here from the plain [`IngestionQuotaConfig`] the config block
    /// carries: the buckets are domain state with no infra dependency, so
    /// there is nothing for a bootstrap layer to build and inject.
    ///
    /// **The anonymous caller is a key like any other.**
    /// `SecurityContext::anonymous()` sets `subject_id` to the nil UUID, so
    /// every anonymous caller shares one bucket. Accepted rather than refused
    /// because it is the *conservative* reading: anonymous traffic is bounded
    /// in aggregate at one subject's allowance instead of being handed a fresh
    /// allowance per caller. Refusing an anonymous context before the charge
    /// would add a new rejection to a path that admits one today, which DESIGN
    /// §3.2 does not ask for.
    quota: IngestionQuota,

    /// `[usage_collector].unavailable_retry_after_secs`: the retry delay a
    /// `ServiceUnavailable` carries where nothing else supplied one, applied
    /// by [`lift_domain_error`] at every plugin-dispatch and plugin-host lift
    /// site. Defaults to [`DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS`] via
    /// [`Self::new`], as `max_batch_records` does.
    unavailable_retry_after_secs: u64,

    /// `[usage_collector].target_not_converged_retry_after_secs`: the delay a
    /// `Conflict(TargetNotConverged)` carries, applied by
    /// [`lift_domain_error`] at every site that can raise one. Defaults to
    /// [`DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS`] via [`Self::new`].
    target_not_converged_retry_after_secs: u64,
}

/// The acceptance instant of one request: now, truncated to the microsecond.
///
/// Truncated because every conforming store keeps microseconds (Postgres
/// `timestamptz` does), so a finer stamp would read back different from what
/// was acknowledged. The same instant judges the covered-period bounds, so
/// the stamp and the late-arrival decision cannot disagree.
fn acceptance_instant() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_microsecond(now.microsecond()).unwrap_or(now)
}

impl Service {
    /// Storage-plugin resolution is lazy: no `types-registry` query happens
    /// here, it is deferred to the first dispatch.
    ///
    /// Metrics default to a no-op adapter — production wires the real
    /// OTLP-backed adapter through [`Service::new_with_metrics`]. The Type
    /// Resolver defaults to [`UnavailableDeclarationSource`]: this constructor
    /// has no `types-registry` adapter to build one from without reintroducing
    /// the domain → infra edge `DeclarationSource` exists to prevent, and a
    /// source that never succeeds never populates the cache, so the TTL and
    /// capacity below are inert. The metadata size cap is likewise hard-coded
    /// to [`DEFAULT_METADATA_SIZE_CAP_BYTES`].
    ///
    /// The covered-period bounds default to [`CoveredPeriodBounds::default`],
    /// which reads the same domain constants `UsageCollectorConfig`'s own
    /// defaults read, so the published values live in one place and a
    /// deployment that moves them cannot leave this constructor behind. The
    /// ingestion quota follows the same rule
    /// ([`IngestionQuotaConfig::default`]), so no test can accidentally
    /// exercise an unthrottled gear.
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
        // Inert for the reason the forward resolver above is: a source that
        // never answers, over a no-op mirror.
        let reverse_resolver = Arc::new(MeterReverseResolver::new(
            Arc::new(UnavailableDeclarationSource),
            Arc::new(NoopDeclarationMirror),
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
            DEFAULT_MAX_BATCH_RECORDS,
            IngestionQuotaConfig::default(),
            DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS,
            DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS,
            reverse_resolver,
        )
    }

    /// Construct the service with an explicit operational-metrics sink, a
    /// pre-built Type Resolver, and the configured metadata size cap.
    ///
    /// Used at gear bootstrap (`module.rs`), which builds the resolver via
    /// [`crate::infra::types_registry_source::build_default_resolvers`] over
    /// the configured `[usage_collector]` cache knobs, and by tests that need
    /// a real metrics adapter, a resolver over a fake `DeclarationSource`, or
    /// a non-default size cap.
    ///
    /// Taking the finished `Arc<TypeResolver>` here, rather than raw cache
    /// knobs or a `DeclarationSource`, keeps this domain module free of any
    /// dependency on the concrete `types-registry` adapter — as `metrics` is
    /// injected as a finished `Arc<dyn UsageCollectorMetrics>`. Every other
    /// parameter is the plain value the config carries rather than the whole
    /// `UsageCollectorConfig`: `metadata_size_cap_bytes`,
    /// `covered_period_bounds` (as `UsageCollectorConfig` projects it),
    /// `max_batch_records`, the `[usage_collector.ingestion_quota]` block
    /// verbatim, and the two DESIGN §3.8 retry-delay keys. The quota buckets
    /// ([`crate::domain::quota::IngestionQuota`]) are domain state with no
    /// infra dependency, so there is no adapter for a bootstrap layer to
    /// build. [`Service::new`] supplies a default for each; production
    /// bootstrap threads the configured values through explicitly.
    #[must_use]
    // Every parameter is a distinct, independently-configured bootstrap input
    // (see the doc above); grouping any subset into a struct would just move
    // the arity to that struct's own constructor, and the plain-config
    // parameters come from unrelated `[usage_collector]` keys that no single
    // type names together.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_metrics(
        hub: Arc<ClientHub>,
        vendor: String,
        enforcer: PolicyEnforcer,
        metrics: Arc<dyn UsageCollectorMetrics>,
        type_resolver: Arc<TypeResolver>,
        metadata_size_cap_bytes: usize,
        covered_period_bounds: CoveredPeriodBounds,
        max_batch_records: usize,
        ingestion_quota: IngestionQuotaConfig,
        unavailable_retry_after_secs: u64,
        target_not_converged_retry_after_secs: u64,
        reverse_resolver: Arc<MeterReverseResolver>,
    ) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            enforcer,
            metrics,
            type_resolver,
            reverse_resolver,
            metadata_size_cap_bytes,
            covered_period_bounds,
            max_batch_records,
            quota: IngestionQuota::new(ingestion_quota),
            unavailable_retry_after_secs,
            target_not_converged_retry_after_secs,
        }
    }

    /// The per-request entry cap this service enforces.
    #[must_use]
    pub const fn max_batch_records(&self) -> usize {
        self.max_batch_records
    }

    /// Record the structural batch cap's request-wide refusal:
    /// `uc_ingestion_requests_total{outcome="rejected",
    /// error_category="validation"}` — and this operation's one structured log
    /// entry (`inst-log-one-per-operation`).
    ///
    /// **Public rather than private to the cap arm** because the REST edge
    /// carries its own copy of the same cap, ahead of the per-entry decode loop
    /// (`handlers/usage_records.rs`'s `dispatch_usage_record_batch`). That
    /// refusal returns before [`Self::create_usage_records`] is entered, so the
    /// counter is required on **both** arms. Both call this one method, so
    /// there is a single recorder — which is also why a submission rejected at
    /// the REST edge still logs exactly once.
    ///
    /// **Telemetry only.** Counting here admits and refuses nothing; the
    /// authoritative check lives in the domain service, as DESIGN §3.2
    /// requires, "because a handler-side check would miss every in-process
    /// caller". The per-record counter is deliberately untouched: the cap runs
    /// ahead of the decode loop, so no entry has been decoded and §3.11.5
    /// counts decoded entries only. `entry_type` is also unvalidated, which is
    /// why the log's `resource` and `referenced_type` are absent — that second
    /// reason is specific to this arm, while the scope rule is the general one
    /// (a rejection inside the per-entry DTO fold has its `entry_type` in hand
    /// and is still uncounted here).
    ///
    /// `op` names which route rejected (`PdpOp::Ingest` / `PdpOp::Backfill`),
    /// threaded in because `uc_ingestion_requests_total` carries no `origin`
    /// label, so only the log's `operation` field distinguishes the two.
    pub fn record_structural_cap_rejection(&self, ctx: &SecurityContext, op: PdpOp) {
        self.metrics.record_ingestion_request(
            IngestRequestOutcome::Rejected,
            IngestRequestErrorCategory::Validation,
        );
        let tenant = ctx.subject_tenant_id().to_string();
        log_operation_completed(
            OperationLogEntry::new(
                op,
                IngestRequestOutcome::Rejected,
                IngestRequestErrorCategory::Validation,
            )
            .tenant(Some(tenant.as_str())),
        );
    }

    /// Charge `submitted` entries against the calling subject's ingestion
    /// allowance as of `now` (DESIGN §3.2, Ingestion Admission Control).
    ///
    /// **Cost is the submitted entry count**, never one per request: a
    /// per-request cost would let a caller take `max_batch_records` times its
    /// budget by batching. A one-entry batch passes `1` because it submitted
    /// one entry, not because a request costs one.
    ///
    /// **Called after the entry cap and before the PDP call**, on the
    /// ingestion path, and §3.2 calls that order load-bearing: the cap bounds
    /// `submitted` at `max_batch_records`, which — with the startup rule that
    /// `burst_entries >= max_batch_records` — is what keeps the rejection's
    /// retry delay finite, and which disposes of the empty submission before
    /// it can cost nothing. Charging before the PDP means charging on
    /// submitted entries, before it is known how many are valid, authorized or
    /// collapsed by dedup; that is deliberate, because the flood being bounded
    /// is submitted volume.
    ///
    /// `now` is the request's single seeded instant, passed in rather than
    /// read here. The bucket's idle sweep treats a `now` behind the last sweep
    /// as *due* (`crate::domain::quota`'s `sweep_if_due`), so per-caller clock
    /// reads that could reorder under concurrency would force a sweep on every
    /// charge and defeat the amortisation the throttle exists for.
    ///
    /// Neither `uc_ingestion_records_total` nor `uc_ingestion_requests_total`
    /// is touched here. The per-record counter must not move at all on a quota
    /// rejection — the charge happens before `entry_type` is validated, so
    /// that counter's required label is unknown (DESIGN §3.11.5) — and the
    /// request counter is the ingestion path's to record in its own quota arm,
    /// because this shared charge cannot know a submission's outcome shape.
    // @cpt-dod:cpt-cf-usage-collector-dod-quota-subject-keyed:p2
    async fn charge_ingestion_quota(
        &self,
        ctx: &SecurityContext,
        submitted: usize,
        now: OffsetDateTime,
    ) -> Result<(), UsageCollectorError> {
        let subject = ctx.subject_id();
        // Lossless on every target this gear builds for; the saturation is
        // the lint obligation, not a reachable case — `submitted` is bounded
        // by the entry cap on the batch path and is literally 1 on the other.
        let cost = u64::try_from(submitted).unwrap_or(u64::MAX);
        let charged = self.quota.charge(subject, cost, now).await;

        // Sampled on every charge, admitted or refused, because the gauge
        // measures residency rather than rejections: a flood of distinct
        // subjects that is entirely *admitted* is the map growth DESIGN
        // §3.11.5 says this gauge watches. Read after the charge's own insert
        // and sweep, so it publishes the residency this request left behind.
        let active = self.quota.active_buckets().await;
        self.metrics
            .set_quota_buckets_active(u64::try_from(active).unwrap_or(u64::MAX));

        let Err(rejection) = charged else {
            return Ok(());
        };

        // By the submitted entry count, not by one: this counter carries
        // throttled volume (DESIGN §3.11.5).
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-rejected-volume-countable:p2
        self.metrics.record_quota_rejection(cost);
        // The subject and tenant cannot be metric labels — §3.11.5's
        // cardinality rule names this quota as its own worked example — so
        // the structured log is where "which caller is being throttled" is
        // answered, deliberately rather than as a gap.
        tracing::warn!(
            subject_id = %subject,
            tenant_id = %ctx.subject_tenant_id(),
            submitted,
            allowance = rejection.allowance,
            retry_after_seconds = rejection.retry_after_seconds,
            "ingestion quota exceeded; submission rejected whole",
        );
        // @cpt-algo:cpt-cf-usage-collector-algo-quota-throttle-outcome:p2
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-whole-rejection:p2
        Err(UsageCollectorError::ingestion_quota_exceeded(
            IngestionQuotaExceededArgs {
                allowance: rejection.allowance,
                submitted,
                retry_after_seconds: rejection.retry_after_seconds,
            },
        ))
    }

    /// The first two steps of both ingestion paths: project the submission
    /// into its persisted shape, then admit or refuse its covered period.
    ///
    /// One function rather than a copy per path, because the two are obliged to
    /// agree. The batch path judges every entry of one submission against a
    /// single `now`, and taking that instant as a parameter is what makes that
    /// structural rather than conventional. Both failures are per-submission:
    /// the projection's own period preconditions
    /// (`cpt-cf-usage-collector-adr-record-identity-derivation` — a bound finer
    /// than the microsecond, or an inverted period) and the path's tolerances
    /// alike surface at one entry, never at the batch.
    ///
    /// **Choosing the projection is this gateway's job, on the submission's
    /// declared `entry_type`.** The two projections differ in exactly what only
    /// a gateway can supply: a withdrawal's `invalidates`, derived here from
    /// the submission's own identity inputs ([`derive_invalidation_target`])
    /// rather than taken from the caller. Reading the declared kind is also
    /// what keeps the SDK's `Internal` guards unreachable in correct operation.
    ///
    /// The derivation runs before the projection, so a withdrawal's target
    /// identifier is computed before the covered period is validated. Nothing
    /// escapes: the value is an argument to the projection that rejects a bad
    /// period, so a refused submission discards it unread — see
    /// [`derive_invalidation_target`].
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::InvalidArgument`] from the projection, on a
    ///   sub-microsecond or inverted covered period, on a missing
    ///   idempotency key, or when the submission's `entry_type` and
    ///   `reason_code` disagree.
    /// * [`UsageCollectorError::InvalidArgument`] with reason
    ///   `FUTURE_WINDOW` / `PAST_WINDOW` when the period ends outside this
    ///   path's tolerances — see [`enforce_covered_period_bounds`].
    /// * [`UsageCollectorError::Internal`] when a projection refuses a
    ///   self-consistent submission as not its own. That is a host-contract
    ///   breach rather than an emitter fault, and the match below is what
    ///   makes it unreachable; it is listed because the type admits it, not
    ///   because a caller can provoke it.
    ///
    /// `now` is also the entry's `accepted_at`.
    ///
    /// **This is the one identity-derivation choke point
    /// (`cpt-cf-usage-collector-algo-entry-identity-derivation`,
    /// `inst-identity-choke-point`):** every entry of every route passes
    /// through here, and this function supplies the two gear-controlled inputs
    /// — `origin` (hardcoded per route, never derived from the entry) and `now`
    /// (seeded once per request). The identity itself is derived in the SDK —
    /// see [`usage_collector_sdk::derive_usage_record_id`].
    ///
    /// The same `project()` call enforces the key/entry-type/reason-code
    /// structural rules
    /// (`cpt-cf-usage-collector-algo-entry-structural-validation`,
    /// `cpt-cf-usage-collector-dod-mandatory-idempotency-key`) and stamps the
    /// server-assigned `id`, `accepted_at` and `origin`
    /// (`cpt-cf-usage-collector-algo-server-field-stamping`,
    /// `cpt-cf-usage-collector-dod-server-assigned-fields`). Neither this
    /// function nor `project()` branches on the sign of `quantity` or on the
    /// declared fold, which is what
    /// `cpt-cf-usage-collector-flow-emit-negative-measurement` requires. Since
    /// the SDK exports the derivation publicly, an external caller reproducing
    /// it offline against the same six inputs obtains the same identifier by
    /// construction (`cpt-cf-usage-collector-flow-reproduce-identity-offline`).
    ///
    /// **This is also the withdrawal-linkage stamping site**
    /// (`cpt-cf-usage-collector-algo-withdrawal-linkage-stamping`,
    /// `cpt-cf-usage-collector-dod-withdrawal-linkage`). The `Invalidation` arm
    /// is the only place in the gear supplying a `target`, and the wire can
    /// supply none: both ingestion shapes carry `deny_unknown_fields` and
    /// publish no `invalidates` property. The stamped value is not an identity
    /// input, and the plugin persists what it is handed rather than deriving
    /// one.
    // @cpt-algo:cpt-cf-usage-collector-algo-entry-structural-validation:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-withdrawal-linkage-stamping:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-withdrawal-linkage:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-entry-identity-derivation:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-server-field-stamping:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-mandatory-idempotency-key:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-dedup-identity-derivation:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-server-assigned-fields:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-emit-negative-measurement:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-reproduce-identity-offline:p1
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    fn project_and_admit(
        &self,
        submission: CreateUsageRecord,
        origin: RecordOrigin,
        now: OffsetDateTime,
    ) -> Result<UsageRecord, UsageCollectorError> {
        let record = match submission.entry_type() {
            EntryType::Record => submission.try_into_usage_record(origin, now)?,
            EntryType::Invalidation => {
                let target = derive_invalidation_target(&submission)?;
                submission.try_into_invalidation_record(origin, now, target)?
            }
        };
        enforce_covered_period_bounds(&self.covered_period_bounds, origin, now, record.window_end)?;
        Ok(record)
    }

    /// Batch ingestion entry
    /// (`cpt-cf-usage-collector-flow-emit-usage-record`).
    ///
    /// The live batch route. Enforces the `1..=max_batch_records` structural
    /// cap (the configured cap; rejected before the pipeline, recorded on
    /// `uc_ingestion_requests_total` as a request-wide `validation` outcome
    /// and not on the per-record counter), observes
    /// `uc_ingestion_batch_size`, delegates to
    /// [`Self::create_usage_records_inner`], and records the completion
    /// telemetry: one `uc_ingestion_records_total` per per-record outcome,
    /// one `uc_ingestion_requests_total` (`accepted` / `partial` /
    /// `rejected`), and `uc_ingestion_duration_seconds` — the first and last
    /// of those carrying `origin="live"`.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::InvalidArgument`] when the input violates the
    ///   `1..=max_batch_records` structural cap (the configured cap).
    /// * [`UsageCollectorError::ServiceUnavailable`] / other variants for a
    ///   batch-level plugin transport / persistence failure.
    ///
    /// # Post-condition
    ///
    /// On `Ok`, the returned vector has length equal to `records.len()` and
    /// preserves input order.
    //
    // @cpt-flow:cpt-cf-usage-collector-flow-emit-usage-record:p1
    pub async fn create_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        // The batch body lives in `create_usage_records_for_origin`, shared
        // with the backfill route so the two cannot drift. `Live` comes from
        // the route this wrapper *is*, not from a default.
        //
        // `records.len()` is the submitted count for an in-process caller:
        // there is no wire decode between the caller and here. A caller that
        // DID filter before calling — the REST handler does — must use
        // `create_usage_records_with_submitted_count` instead.
        let submitted_before_decode = records.len();
        self.create_usage_records_for_origin(
            ctx,
            records,
            RecordOrigin::Live,
            submitted_before_decode,
        )
        .await
    }

    /// [`Self::create_usage_records`] for a caller that decoded a wire
    /// submission itself and dropped the entries that failed to decode, so
    /// `records` is shorter than what the caller actually received.
    ///
    /// `submitted_before_decode` is the entry count the submission **arrived
    /// with**, before any entry was decoded, validated or dropped — not
    /// `records.len()`. Both the structural cap and the ingestion quota are
    /// charged against it, per DESIGN §3.2: *"a caller cannot escape the
    /// charge by submitting entries that fail later"*, and a wire-decode
    /// failure is the earliest kind of "fails later". Passing the post-decode
    /// count would let a caller send `max_batch_records` entries of which one
    /// is well-formed and pay a single token — DESIGN §3.9.4's "ingestion
    /// flood from a single subject", admitted at 1/`max_batch_records` of its
    /// true cost.
    ///
    /// The check stays here rather than moving to the handler, per DESIGN
    /// §3.2: *"a handler-side check would miss every in-process caller"*.
    /// That constrains where the check lives, not which count it is given.
    ///
    /// `records` may be **empty** while `submitted_before_decode` is not:
    /// that is a submission whose every entry was refused at the wire edge.
    /// It is still charged — see `create_usage_records_for_origin`, which
    /// returns an empty result set for it without entering the pipeline.
    ///
    /// # Errors
    ///
    /// The same variants as [`Self::create_usage_records`], with the cap and
    /// the quota judged against `submitted_before_decode`.
    ///
    /// # Post-condition
    ///
    /// On `Ok`, one result slot per entry of `records`, in input order —
    /// `records.len()` slots, not `submitted_before_decode` of them. The
    /// caller owns the slots for the entries it dropped.
    pub async fn create_usage_records_with_submitted_count(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
        submitted_before_decode: usize,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        self.create_usage_records_for_origin(
            ctx,
            records,
            RecordOrigin::Live,
            submitted_before_decode,
        )
        .await
    }

    /// [`Self::backfill_usage_records`] for a caller that decoded a wire
    /// submission itself — the import route's
    /// [`Self::create_usage_records_with_submitted_count`], and owing the
    /// same obligation for the same reason: backfill draws the same
    /// allowance as live emission, so a cheap escape on one route is a
    /// cheap escape on both.
    ///
    /// # Errors
    ///
    /// The same variants as [`Self::backfill_usage_records`].
    ///
    /// # Post-condition
    ///
    /// As [`Self::create_usage_records_with_submitted_count`].
    pub async fn backfill_usage_records_with_submitted_count(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
        submitted_before_decode: usize,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        self.create_usage_records_for_origin(
            ctx,
            records,
            RecordOrigin::Backfill,
            submitted_before_decode,
        )
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
    /// batch: a withdrawal of a period older than the live past tolerance
    /// belongs here. `origin` records the route each entry travelled and is
    /// not one of the fields the faithful-copy rule compares, so withdrawing
    /// a `live` target here is the ordinary case rather than a mismatch.
    ///
    /// An entry whose covered period ends further back than the configured
    /// backfill window is authorized against
    /// `usage_record::actions::BACKFILL` instead of `CREATE`
    /// ([`ingestion_action`]). One batch may mix the two; every entry of
    /// it is labelled `operation="backfill"` on the PDP instruments either
    /// way — see [`PdpOp::Backfill`], which owns that collision.
    ///
    // TODO(`cpt-cf-usage-collector-nfr-workload-isolation`): this route shares
    // the live path's runtime, connection pool and fan-out budget — the same
    // `create_usage_records_for_origin` body under a different `origin`, with
    // nothing bounding it separately, so a bulk import can still degrade live
    // ingestion p95. Backend pool isolation is separately a plugin deployment
    // obligation. Confirmation needs a concurrent load test against
    // `cpt-cf-usage-collector-nfr-throughput-profile`, which is why no
    // gear-level test goes red while this stands.
    ///
    /// # Errors
    ///
    /// The same variants as [`Self::create_usage_records`].
    ///
    /// # Post-condition
    ///
    /// As [`Self::create_usage_records`]: on `Ok`, one result slot per
    /// input, in input order.
    ///
    /// **Traceability.** This wrapper, [`Self::project_and_admit`] and the
    /// shared pipeline realize `cpt-cf-usage-collector-dod-backfill-route` and
    /// `cpt-cf-usage-collector-algo-backfill-origin-marking` /
    /// `cpt-cf-usage-collector-dod-backfill-origin-marker`: `origin` is the
    /// fixed [`RecordOrigin::Backfill`] constant this wrapper passes, never
    /// derived from an entry's period or kind, and an absorbed repeat returns
    /// the stored entry's own marker via `resolve_follower`.
    ///
    /// **Does NOT realize**, named here rather than left implicit:
    ///
    /// - `cpt-cf-usage-collector-algo-backfill-window-bound` /
    ///   `cpt-cf-usage-collector-dod-backfill-window-bound` — no hard rejection
    ///   of an over-aged period exists anywhere in this gear. The documents
    ///   assert the bound and this gear's config, route docs and tests
    ///   contradict them; see [`ingestion_action`] for the citation trail.
    ///   Whether the bound is buildable depends on the unresolved model
    ///   question below.
    /// - `cpt-cf-usage-collector-algo-backfill-workload-isolation` /
    ///   `cpt-cf-usage-collector-dod-backfill-workload-isolation` — the TODO
    ///   immediately above this doc comment.
    /// - `cpt-cf-usage-collector-algo-backfill-request-admission` /
    ///   `cpt-cf-usage-collector-dod-backfill-permission` — both require the
    ///   PDP action to be selected from the route alone; [`ingestion_action`]
    ///   selects it per entry from `window_end` instead.
    ///
    /// **The last two are halves of one unresolved design conflict**: DESIGN
    /// wants the route to pick the action and the window to be a hard bound;
    /// this gear has the window pick the action and bounds nothing. Under this
    /// gear's model the `BACKFILL` arm is unreachable beneath a bound; under
    /// DESIGN's it stays reachable and the bound is buildable. See
    /// [`ingestion_action`].
    // @cpt-dod:cpt-cf-usage-collector-dod-backfill-route:p2
    // @cpt-algo:cpt-cf-usage-collector-algo-backfill-origin-marking:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-backfill-origin-marker:p2
    pub async fn backfill_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        // `Backfill` comes from the route this wrapper *is*; everything else
        // is the live route's own body, which keeps the two from drifting.
        // `records.len()` is the submitted count for an in-process caller — a
        // gateway that decoded a wire submission first calls
        // `backfill_usage_records_with_submitted_count`.
        let submitted_before_decode = records.len();
        self.create_usage_records_for_origin(
            ctx,
            records,
            RecordOrigin::Backfill,
            submitted_before_decode,
        )
        .await
    }

    /// The batch ingestion body, parameterized by the path that admitted the
    /// submission: the structural `1..=max_batch_records` cap (the
    /// configured cap), the pipeline call, and the completion telemetry
    /// alike.
    ///
    /// DESIGN §3.2 makes the backfill path "the same component under
    /// workload isolation: identical validation ... and `origin = backfill`",
    /// so `origin` is the whole difference between the two batch entry points
    /// and they share this body rather than each carrying a copy of the
    /// completion telemetry.
    ///
    /// **Two counts, and they are not interchangeable.**
    /// `submitted_before_decode` is what the caller *sent*; `records.len()`
    /// is what survived the caller's own decode. Both admission gates — the
    /// structural cap and the ingestion quota — read the first, so a caller
    /// cannot shrink either one by sending entries that fail early. Only the
    /// pipeline and the per-record telemetry read the second. For every
    /// in-process caller the two are the same number.
    ///
    /// **An admitted all-malformed submission (`records` empty,
    /// `submitted_before_decode` not) is a stated four-way split, not an
    /// omission.** It is charged like any other submission and records three
    /// of the four ingestion instruments — `uc_ingestion_requests_total`
    /// (`outcome=partial`), `uc_ingestion_batch_size` and
    /// `uc_ingestion_duration_seconds{origin}` — from the early return below.
    /// `uc_ingestion_records_total` stays silent: §3.11.5 scopes it to
    /// decoded entries and this submission has none.
    ///
    /// **A partly-malformed submission is the same rule applied to a mixed
    /// batch.** The per-record counter covers the entries that decoded; the
    /// request counter does not read only those, because
    /// `submitted_before_decode` exceeding the decoded count means the
    /// caller's own fold refused entries this method never saw, and the
    /// submission completes as `partial` on that evidence alone. Deciding it
    /// from the pipeline's verdicts instead reports `accepted` behind an HTTP
    /// 207.
    // @cpt-flow:cpt-cf-usage-collector-flow-authorize-ingestion:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-ingestion-request-admission:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-batch-outcome-model:p1
    async fn create_usage_records_for_origin(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
        origin: RecordOrigin,
        submitted_before_decode: usize,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        let start = std::time::Instant::now();
        debug_assert!(
            records.len() <= submitted_before_decode,
            "a caller dispatched {} entries while claiming {submitted_before_decode} were \
             submitted; the submitted count is the pre-decode one and cannot be the smaller",
            records.len(),
        );
        // This operation's one request-level identifier for every completion
        // point below: a batch has no single resource or referenced type
        // (`inst-log-identifiers-here` carries those only where the operation
        // has exactly one), but it always has exactly one attributed tenant.
        let tenant = ctx.subject_tenant_id().to_string();
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-request-reject
        // The cap judges what was SUBMITTED, not what decoded — the same
        // number the REST edge already caps on
        // (`api/rest/handlers/usage_records.rs`), so the edge check and this
        // one cannot disagree, and the quota below is bounded by the same
        // value the cap bounded.
        let actual = submitted_before_decode;
        if actual == 0 || actual > self.max_batch_records {
            // Request-wide validation rejection; the yaml calls this
            // "rejected whole with 400 on `records`". The per-record counter
            // is NOT touched: entry_type is not yet validated, so its required
            // label is unknown. Routed through
            // `record_structural_cap_rejection` because the REST edge's copy
            // of this cap counts the same point.
            self.record_structural_cap_rejection(ctx, pdp_op_for(origin));
            return Err(UsageCollectorError::invalid_batch_size(
                actual,
                1,
                self.max_batch_records,
            ));
        }
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-request-reject

        // The quota, after the cap and before the PDP call (DESIGN §3.2).
        // Seeded here rather than inside `create_usage_records_inner`, which
        // now takes it: the charge and every entry's covered-period admission
        // must read one instant, and the cap above is what bounds `actual`
        // so the charge can never cost more than the bucket can hold.
        //
        // `now` is read on this side of the cap check, not before it: an
        // over-cap submission is refused without a clock read, which is one
        // fewer thing a flood of rejected requests makes the gear do.
        let now = acceptance_instant();
        // @cpt-flow:cpt-cf-usage-collector-flow-quota-throttled-submission:p2
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-charge-order:p2
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-all-paths:p2
        if let Err(err) = self.charge_ingestion_quota(ctx, actual, now).await {
            // The request counter is this path's to record: the shared charge
            // cannot record an outcome it does not own. `quota` and the cap's
            // `validation` are different values of the same label, which is
            // what tells the two whole-request refusals apart where the HTTP
            // status alone cannot.
            self.metrics.record_ingestion_request(
                IngestRequestOutcome::Rejected,
                IngestRequestErrorCategory::Quota,
            );
            log_operation_completed(
                OperationLogEntry::new(
                    pdp_op_for(origin),
                    IngestRequestOutcome::Rejected,
                    IngestRequestErrorCategory::Quota,
                )
                .tenant(Some(tenant.as_str())),
            );
            // Returns ahead of the batch-size observation below for the same
            // reason the cap rejection does: a submission refused whole never
            // entered per-record processing, and an observation here would
            // put throttled volume into a histogram that describes admitted
            // batches.
            return Err(err);
        }

        // Computed once so the two batch-size observations below (the
        // all-malformed early return's and the ordinary path's) share one
        // source of truth. The duration read is deliberately NOT hoisted
        // alongside it: it genuinely differs between the two paths, so it
        // stays a `start.elapsed()` read at each point of emission.
        let submitted_as_u64 = u64::try_from(actual).unwrap_or(u64::MAX);

        // Every entry was refused before it got here — a wire submission whose
        // entries all failed to decode. It has still been charged: DESIGN §3.2,
        // "a caller cannot escape the charge by submitting entries that fail
        // later", has no exemption for failing *first*.
        //
        // Nothing is dispatched: there is no plugin to resolve for zero
        // entries, and resolving one would turn an all-malformed submission
        // into a 503 whenever the storage plugin happened to be down.
        //
        // **Three of the four ingestion instruments are recorded here, and the
        // fourth is silent on purpose** — do not "fix" it:
        //
        // - `uc_ingestion_requests_total` (request-wide): recorded,
        //   `outcome=partial` (defined as "at least one per-record rejection"),
        //   `error_category=none`.
        // - `uc_ingestion_batch_size` (samples the **submitted** count):
        //   recorded with `actual`, the number the cap and quota were judged
        //   against.
        // - `uc_ingestion_duration_seconds{origin}`: recorded. `origin` is a
        //   property of the route rather than the payload, so it is known even
        //   though no entry decoded, and this submission did real work — the
        //   cap check and the quota charge — whose cost the histogram measures.
        // - `uc_ingestion_records_total` (per-record): **not** recorded, and
        //   must not be. §3.11.5 scopes it to decoded entries, and this
        //   submission has none. The reason is the instrument's scope, not a
        //   missing label: the fold parses `entry_type` first
        //   (`record_request_into_domain`), so most fold rejections do have one.
        //
        // What an operator sees instead for fold rejections: the shortfall of
        // `uc_ingestion_batch_size` against the per-record counter is the
        // fold-rejection volume, and `uc_ingestion_requests_total` reports
        // `partial` whenever a submission carried any rejection.
        if records.is_empty() {
            self.metrics.observe_ingestion_batch_size(submitted_as_u64);
            self.metrics.record_ingestion_request(
                IngestRequestOutcome::Partial,
                IngestRequestErrorCategory::None,
            );
            self.metrics
                .observe_ingestion_duration(start.elapsed().as_secs_f64(), origin);
            log_operation_completed(
                OperationLogEntry::new(
                    pdp_op_for(origin),
                    IngestRequestOutcome::Partial,
                    IngestRequestErrorCategory::None,
                )
                .tenant(Some(tenant.as_str())),
            );
            return Ok(Vec::new());
        }

        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-observe-batch-size
        // Samples the submitted count, like the two gates above, and not the
        // post-decode one — otherwise the histogram would describe a smaller
        // submission than the quota charged for.
        //
        // **DESIGN is silent on received-versus-decoded** — its one mention of
        // `uc_ingestion_batch_size` is a histogram row and nothing more. The
        // governing contract is the gear's own instrument text, "one
        // observation per received batch submission, before per-record
        // processing" ([`UsageCollectorMetrics::observe_ingestion_batch_size`]).
        //
        // Sampling the submitted count is also what makes this instrument the
        // one that carries fold-rejection volume: this figure minus
        // `uc_ingestion_records_total`'s total over the same window IS the
        // entries a wire caller's fold refused. The subtraction only works
        // because both ends read a count the other cannot shrink.
        //
        // An all-malformed submission returns above and never reaches this
        // line, but is not unobserved: the empty-dispatch branch samples this
        // same instrument with this same value, so the identity holds there
        // too.
        self.metrics.observe_ingestion_batch_size(submitted_as_u64);
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-observe-batch-size

        // `entry_type` is captured per input index before `records` is moved
        // into the inner pipeline (the per-entry counter needs it after).
        let entry_types: Vec<EntryType> =
            records.iter().map(CreateUsageRecord::entry_type).collect();

        let result = self
            .create_usage_records_inner(ctx, records, origin, now)
            .await;
        let seconds = start.elapsed().as_secs_f64();

        if let Ok(per_record) = &result {
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-records-counter
            for (record_result, entry_type) in per_record.iter().zip(entry_types.iter().copied()) {
                let (outcome, error_category) = match record_result {
                    Ok(_) => (RecordOutcome::Accepted, RecordErrorCategory::None),
                    Err(e) => (RecordOutcome::Rejected, classify_record_error(e)),
                };
                self.metrics
                    .record_ingestion_record(outcome, entry_type, origin, error_category);
            }
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-records-counter
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-request-completion-metrics
            // **Two sources of rejection, and this counter answers for
            // both.** `per_record` carries the pipeline's verdicts; it does
            // not carry the entries a wire caller's own DTO fold already
            // refused, which the REST handler composes into its 207 envelope
            // without ever dispatching them. `submitted_before_decode`
            // exceeding the decoded count IS that second population, and it
            // is the only evidence of it this method receives — reading
            // `per_record` alone reports `accepted` while the route answers
            // 207, against this counter's own `outcome` vocabulary. For an
            // in-process caller the two counts are equal by construction, so
            // this disjunct fires only for a caller that decoded a wire
            // submission itself.
            let refused_before_decode = submitted_before_decode > entry_types.len();
            let request_outcome = if refused_before_decode || per_record.iter().any(Result::is_err)
            {
                IngestRequestOutcome::Partial
            } else {
                IngestRequestOutcome::Accepted
            };
            self.metrics
                .record_ingestion_request(request_outcome, IngestRequestErrorCategory::None);
            log_operation_completed(
                OperationLogEntry::new(
                    pdp_op_for(origin),
                    request_outcome,
                    IngestRequestErrorCategory::None,
                )
                .tenant(Some(tenant.as_str())),
            );
        } else {
            // Whole-request plugin failure (`inst-emit-batch-spi-fail-mark`);
            // the structural cap-check above records its own rejection
            // and returns before this arm is ever reached.
            self.metrics.record_ingestion_request(
                IngestRequestOutcome::Rejected,
                IngestRequestErrorCategory::PluginError,
            );
            log_operation_completed(
                OperationLogEntry::new(
                    pdp_op_for(origin),
                    IngestRequestOutcome::Rejected,
                    IngestRequestErrorCategory::PluginError,
                )
                .tenant(Some(tenant.as_str())),
            );
        }
        self.metrics.observe_ingestion_duration(seconds, origin);
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-request-completion-metrics
        result
    }

    /// Create a batch of `UsageRecord`s through the ingestion path per
    /// `cpt-cf-usage-collector-flow-emit-usage-record`.
    ///
    /// Per-record stages — identity derivation, PDP authorization, and SPI
    /// dispatch — run independently for each input; eligible records carry
    /// their caller-supplied covered period through to persistence
    /// UTC-normalized but otherwise unchanged, nothing quantized or truncated,
    /// and are dispatched together. Per-record validation / SPI failures
    /// surface in the result vector at their input index — including a
    /// rejected covered period, which
    /// `cpt-cf-usage-collector-adr-record-identity-derivation` makes a
    /// per-submission precondition of the identity derivation. The outer `Err`
    /// is reserved for batch-level failures (plugin handle resolution, outer
    /// SPI dispatch, and the structural batch-size cap).
    ///
    /// The SDK-facing batch cap of `1..=max_batch_records` is enforced here,
    /// not at the REST handler, which is a thin wrapper over this entry.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::InvalidArgument`] when the input violates
    ///   the `1..=max_batch_records` structural cap (the configured cap).
    /// * [`UsageCollectorError::ServiceUnavailable`] when the storage plugin
    ///   handle cannot be resolved or the outer SPI dispatch fails.
    /// * Any other [`UsageCollectorError`] variant lifted from a batch-level
    ///   plugin transport / persistence failure.
    ///
    /// Per-record failures (a rejected covered period, authorization denial,
    /// an unresolvable declaration, malformed metadata, SPI errors against
    /// individual records) surface in the per-index `Result` entries of the
    /// returned vector rather than the outer `Err`. "A rejected covered
    /// period" covers both of [`Self::project_and_admit`]'s refusals, and
    /// every entry of one batch is judged against a single `now`, so two
    /// entries carrying the same period cannot be decided differently.
    ///
    /// `origin` is the caller's route, not a caller's value: the wrapper that
    /// *is* a route passes its own. One value for the whole batch — a batch is
    /// admitted by one route.
    ///
    /// # Post-condition
    ///
    /// On `Ok`, the returned vector has length equal to `records.len()` and
    /// preserves input order: index `i` of the output corresponds to index
    /// `i` of the input batch.
    //
    // Realizes the batch flow `cpt-cf-usage-collector-flow-emit-usage-record`,
    // the only ingestion pipeline in the gear.
    //
    // The workload-isolation DoD below is the write-vs-read isolation
    // obligation, which this body satisfies: the gateway is the sole write
    // entry point and shares no state with the query gateway. The
    // backfill-vs-live gap is a DIFFERENT obligation, owned by no feature file
    // and tracked at `Self::backfill_usage_records`; this marker does not
    // cover it.
    // @cpt-dod:cpt-cf-usage-collector-nfr-workload-isolation:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-ingestion:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-record-metadata:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-resource-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-subject-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-ingestion-authorization:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-usage-type-resolution:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-tenant-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-pluggable-storage:p1
    // @cpt-dod:cpt-cf-usage-collector-constraint-no-business-logic:p1
    // @cpt-dod:cpt-cf-usage-collector-component-ingestion-gateway:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-idempotency:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-idempotency-by-key:p1
    // @cpt-dod:cpt-cf-usage-collector-adr-mandatory-idempotency:p1
    // @cpt-dod:cpt-cf-usage-collector-adr-caller-supplied-attribution:p1
    // @cpt-dod:cpt-cf-usage-collector-seq-emit-usage:p1
    // @cpt-dod:cpt-cf-usage-collector-entity-model:p1
    // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-receive-ctx
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    async fn create_usage_records_inner(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
        origin: RecordOrigin,
        now: OffsetDateTime,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-algo-attrib-receive-ctx
        // The `1..=max_batch_records` structural cap (the configured cap) is
        // enforced by the public `create_usage_records` wrapper (before the
        // batch-size observation), so callers of the inner path are already
        // in range.

        let submission_count = records.len();
        let mut results: Vec<Option<Result<UsageRecord, UsageCollectorError>>> =
            (0..submission_count).map(|_| None).collect();
        // Per-input-index admissibility, cleared by any pass that finishes a
        // slot: the covered-period conversion below and the PDP projection
        // further down.
        let mut pdp_allowed: Vec<bool> = vec![true; submission_count];

        // The submission each withdrawal was built from, kept per input index
        // because the projection consumes the submission the faithful-copy
        // comparator runs against. `Some` exactly when the entry declares
        // `entry_type: invalidation`, so the comparator runs against what the
        // caller actually sent. The withdrawal's other half — the reason
        // paired with the derived target — is read back off the projected
        // entry instead, so there is no second copy to build inconsistently.
        let mut withdrawals: Vec<Option<CreateUsageRecord>> = Vec::with_capacity(submission_count);

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
        // `now` arrives as a parameter, captured once for the whole batch by
        // `create_usage_records_for_origin`: two entries carrying the same
        // covered period must not be judged differently because the clock
        // crossed the bound between them, and the quota charge and the
        // admission must read one instant.
        let mut derived: Vec<(usize, UsageRecord)> = Vec::with_capacity(submission_count);
        for (index, submission) in records.into_iter().enumerate() {
            withdrawals.push(
                matches!(submission.entry_type(), EntryType::Invalidation)
                    .then(|| submission.clone()),
            );
            // Per-submission, at its own input index — never a batch-level
            // failure, or one out-of-bounds entry would discard a whole
            // import.
            match self.project_and_admit(submission, origin, now) {
                Ok(record) => derived.push((index, record)),
                Err(e) => {
                    results[index] = Some(Err(e));
                    // Redundant today — every later pass walks `derived` — but
                    // "this slot is finished" must not be spelled two ways: a
                    // pass added later that consults `pdp_allowed` alone would
                    // otherwise read a rejected slot as admissible.
                    pdp_allowed[index] = false;
                }
            }
        }

        let plugin = self
            .resolve_plugin_for(PluginOp::CreateUsageRecords)
            .await?;

        let mut eligible: Vec<(usize, MeterRef, UsageRecord)> = Vec::new();
        let mut pending_targets: Vec<PendingInvalidationTarget> = Vec::new();

        // Every derived id in this request, so a withdrawal naming another entry
        // of the same batch is told its target has not converged rather than
        // that it does not exist.
        let batch_ids: HashSet<Uuid> = derived.iter().map(|(_, record)| record.id).collect();

        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-authorize
        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-pdp
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
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-pdp

        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-reject
        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-allow
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-pdp-projected-deny
        // A permitted tuple group that carries an invalidation keeps its scope,
        // compiled once, for the target lookups;
        // a denied/unavailable group is projected onto `results` here.
        let mut lookup_scopes = project_pdp_decisions(
            pdp_decisions,
            &withdrawals,
            submission_count,
            &mut results,
            &mut pdp_allowed,
        );
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-pdp-projected-deny
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-allow
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-reject
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-authorize

        let distinct_gts_type_ids: HashSet<MeterTypeId> = derived
            .iter()
            .filter(|(idx, _)| pdp_allowed[*idx])
            .map(|(_, r)| r.gts_type_id.clone())
            .collect();

        // The fan-out lifts each per-id outcome to `DomainError` eagerly so
        // the cached value is Clone and one resolution projects to every input
        // index referencing that gts_type_id without re-resolving it.
        let declaration_cache: DeclarationCache =
            stream::iter(distinct_gts_type_ids.into_iter().map(|gts_type_id| {
                let type_resolver = self.type_resolver.as_ref();
                async move {
                    // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-resolve
                    let outcome = type_resolver.resolve(&gts_type_id).await;
                    // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-resolve
                    (gts_type_id, outcome)
                }
            }))
            .buffer_unordered(TYPE_RESOLUTION_FANOUT_CONCURRENCY)
            .collect()
            .await;

        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-foreach
        for (index, record) in derived {
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-pdp
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-deny
            if !pdp_allowed[index] {
                continue;
            }
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-deny
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-pdp

            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-unknown-usage-type
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
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-unknown-usage-type

            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-semantics
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-semantics-invalid
            // **Keyed on the projected entry, not on the kept submission**:
            // the entry that gets dispatched is this one, so an absent
            // submission is a breach refused here rather than a fall-through
            // to `eligible.push` with nothing having checked the target. The
            // target read is deferred to a post-loop dedup + bounded fan-out
            // pre-pass (`inst-algo-semantics-l1-dedup` /
            // `inst-algo-semantics-l1-bounded-fanout`), which also runs the
            // metadata check after the target check.
            if let Some(invalidation) = record.invalidation.clone() {
                let Some(submission) = withdrawals[index].take() else {
                    results[index] = Some(Err(invariant_breach(format!(
                        "the projected invalidation at input {index} has no submission kept for it"
                    ))));
                    continue;
                };
                let (lookup_scope_id, lookup_scope) = match lookup_scopes[index].take() {
                    Some(Ok(scope)) => scope,
                    Some(Err(e)) => {
                        results[index] = Some(Err(UsageCollectorError::from(e)));
                        continue;
                    }
                    None => {
                        results[index] = Some(Err(invariant_breach(format!(
                            "no permit scope was kept for the invalidation at input {index}"
                        ))));
                        continue;
                    }
                };
                pending_targets.push(PendingInvalidationTarget {
                    index,
                    submission,
                    invalidation,
                    meter: MeterRef::new(declaration.type_uuid, record.gts_type_id.clone()),
                    record,
                    lookup_scope,
                    lookup_scope_id,
                });
                continue;
            }
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-semantics-invalid
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-semantics

            // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
            observe_metadata_bytes(self.metrics.as_ref(), &record.metadata);
            // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-observe-bytes
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata-closed-shape
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata-too-large
            if let Err(e) = validate_submit_record_metadata(
                &declaration,
                &record.metadata,
                self.metadata_size_cap_bytes,
            ) {
                results[index] = Some(Err(e));
                continue;
            }
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata-too-large
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-metadata-closed-shape

            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-eligible
            // The covered period is carried onto the persisted entry verbatim:
            // `try_into_usage_record` normalized both bounds to UTC and
            // rejected anything finer than the microsecond, so nothing is
            // truncated here or downstream. The reference the plugin keys
            // storage on is taken from the declaration this entry already
            // resolved — never derived here (`MeterRef`'s own doc).
            let meter = MeterRef::new(declaration.type_uuid, record.gts_type_id.clone());
            eligible.push((index, meter, record));
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-record-eligible
        }
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-foreach

        resolve_invalidation_targets(
            plugin.as_ref(),
            self.metrics.as_ref(),
            pending_targets,
            &declaration_cache,
            self.metadata_size_cap_bytes,
            &mut results,
            &mut eligible,
            &batch_ids,
            self.unavailable_retry_after_secs,
            self.target_not_converged_retry_after_secs,
        )
        .await;

        // The target pre-check pushes verified invalidations to `eligible`
        // after the input-order foreach has completed, so the vec is no
        // longer guaranteed in input-index order. Sort once before the
        // plugin SPI dispatch; per-record results are still routed back
        // via the input index, so this only affects the order in which
        // the plugin sees the entries.
        eligible.sort_by_key(|(index, _, _)| *index);

        // One dispatch per dedup identity (DESIGN §3.1 "Collision
        // resolution"); see `dispatch_eligible_entries`.
        dispatch_eligible_entries(
            plugin.as_ref(),
            self.metrics.as_ref(),
            eligible,
            &mut results,
            self.unavailable_retry_after_secs,
            self.target_not_converged_retry_after_secs,
        )
        .await?;

        // @cpt-begin:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-return
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
        // @cpt-end:cpt-cf-usage-collector-flow-authorize-ingestion:p1:inst-auth-ingest-return
    }

    /// Read a single `UsageRecord` by `uuid` from the bound storage plugin,
    /// scoped to the caller's compiled PDP grant.
    ///
    /// Authorizes FIRST via [`authz::authorize_get_usage_record_scope`] — a
    /// pre-row PDP request (the id-only boundary has no record attribution
    /// fields to offer yet) under `require_constraints(true)`, mirroring
    /// [`Self::list_usage_records`]'s posture. The lookup carries no
    /// caller-supplied filter, so the projected scope IS the whole filter
    /// passed to the SPI: a row outside it is reported `UsageRecordNotFound`
    /// exactly as a non-existent `id` would be, so this surface cannot be used
    /// as an existence oracle. A PDP deny is additionally collapsed into that
    /// same `NotFound` (see [`collapse_deny_to_not_found`]).
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
    //
    // Traceability: realizes `flow-lookup-entry-by-identifier`,
    // `algo-point-lookup-resolution` and `dod-point-lookup-exact-fact` — no
    // type reference, no range, no filter and no paging (`inst-pt-input`); the
    // compiled `scope_expr` is the whole filter (`inst-pt-scope-is-filter`);
    // and an absent entry and an unauthorized one answer identically
    // (`inst-pt-unauthorized-return`). Also the point-lookup half of
    // `algo-period-end-selection` / `dod-period-end-selection`: no range
    // parameter, so no selection predicate applies (`inst-sel-point-lookup`).
    // @cpt-flow:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-point-lookup-resolution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-point-lookup-exact-fact:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-period-end-selection:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-period-end-selection:p1
    //
    // Traceability: one dispatch to the bound plugin, no retry, no
    // session/pool affinity, no replication-watermark wait, and the result
    // passed through unchanged — the point-lookup half of
    // `algo-staleness-defensive-read` and `dod-consistency-no-stronger-read-claim`.
    // @cpt-dod:cpt-cf-usage-collector-dod-consistency-no-stronger-read-claim:p1
    pub async fn get_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UsageRecord, UsageCollectorError> {
        let start = std::time::Instant::now();

        // The PDP step is hoisted out of the body below — still inside
        // `start`'s span, so the duration sample covers it — because this
        // function must hold **two** shapes of one outcome at once: the
        // collapsed one the caller reads, where a deny is indistinguishable
        // from a miss, and the uncollapsed one `uc_query_requests_total` is
        // labelled from. `collapse_deny_to_not_found` discards the PDP's
        // decision, so the decision is read here, before it runs.
        // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-scope
        // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-pdp-deny
        let authorized = authz::authorize_get_usage_record_scope(
            &self.enforcer,
            self.metrics.as_ref(),
            PdpOp::GetRecord,
            ctx,
        )
        .await
        .map_err(UsageCollectorError::from);
        // Lifted first, then tested, so this predicate is the same one
        // `collapse_deny_to_not_found` itself applies — one definition of
        // "this was a deny", not two that can drift apart.
        let pdp_denied = matches!(
            authorized,
            Err(UsageCollectorError::PermissionDenied { .. })
        );
        let authorized = authorized.map_err(|e| collapse_deny_to_not_found(e, id));
        // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-pdp-deny
        // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-scope

        let result = async move {
            let scope_expr = authorized?;

            let plugin = self.resolve_plugin_for(PluginOp::GetUsageRecord).await?;

            // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-resolve
            let stored = instrument_spi(
                self.metrics.as_ref(),
                PluginOp::GetUsageRecord,
                plugin.get_usage_record(id, &scope_expr, false),
            )
            .await
            .map_err(|e| {
                lift_domain_error(
                    DomainError::from(e),
                    UnavailableRetryAfterSecs(self.unavailable_retry_after_secs),
                    TargetNotConvergedRetryAfterSecs(self.target_not_converged_retry_after_secs),
                )
            })?;
            // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-resolve

            // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-project
            // This method takes no meter, so the identifier is recovered from
            // the reference the row carries. The resolver's first two tiers
            // answer without touching `types-registry` (slice 1,
            // `domain::meter_reverse`), so the common path adds no dependency
            // to this read.
            //
            // **This is the one re-attachment site that does not compare
            // references first**, and it cannot: there is no expected
            // reference to compare against, because the reference is what the
            // identifier was derived from.
            let gts_type_id = self
                .reverse_resolver
                .resolve(stored.gts_type_uuid)
                .await
                .map_err(|e| match e {
                    // Not `lift_domain_error`'s default, which would answer
                    // 404. The row exists and the caller is entitled to it;
                    // what failed is the gear's ability to name its meter, and
                    // a 404 would hide gear-side corruption behind a
                    // caller-shaped answer.
                    DomainError::DeclarationNotFound { .. } => {
                        UsageCollectorError::internal(format!(
                            "the ledger holds registry reference {} for entry {id}, and it no \
                             longer names a meter type",
                            stored.gts_type_uuid,
                        ))
                    }
                    other => lift_domain_error(
                        other,
                        UnavailableRetryAfterSecs(self.unavailable_retry_after_secs),
                        TargetNotConvergedRetryAfterSecs(
                            self.target_not_converged_retry_after_secs,
                        ),
                    ),
                })?;

            // Passed through exactly as the plugin returned it otherwise: no
            // fold, no marker, no suppression — the point-lookup half of
            // `cpt-cf-usage-collector-algo-raw-ledger-projection`.
            // @cpt-algo:cpt-cf-usage-collector-algo-raw-ledger-projection:p1
            Ok(stored.into_usage_record(gts_type_id))
            // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-project
        }
        .await;

        // F2 (the gear's `docs/DESIGN.md` §3.11.5): `point` is declared on
        // `uc_query_requests_total` and `uc_query_duration_seconds` —
        // `admits_duration_sample` gates the latter so this one call serves
        // both. `observe_query_result_rows` is deliberately not called here:
        // `admits_result_rows_sample` is `false` for `QueryKind::Point`
        // because a point lookup returns one record, not a page, and
        // `uc_query_result_rows` does not declare `point`.
        //
        // **The deny is labelled from `pdp_denied`, not from `result`**:
        // `collapse_deny_to_not_found` has already rewritten a deny into the
        // same `NotFound` a genuine miss produces, and `NotFoundReason`
        // deliberately offers no way to read the distinction back. The bit
        // captured above the body is the only surviving witness, and it is
        // what makes `classify_query_result`'s `UsageRecordNotFound` arm safe
        // to read as a genuine miss. **A second deny-collapsing path into that
        // classifier would reopen this**, which is why the collapse has
        // exactly one caller. The caller surface is unchanged, and
        // `uc_authz_decisions_total{operation="get_record", decision="deny"}`
        // still records the decision inside
        // `authorize_get_usage_record_scope`.
        let seconds = start.elapsed().as_secs_f64();
        let (outcome, error_category) = if pdp_denied {
            (RequestOutcome::Denied, QueryErrorCategory::Authz)
        } else {
            classify_query_result(&result)
        };
        self.metrics
            .record_query_request(QueryKind::Point, outcome, error_category, seconds);
        let log_tenant = ctx.subject_tenant_id().to_string();
        let log_resource = id.to_string();
        log_operation_completed(
            OperationLogEntry::new(PdpOp::GetRecord, outcome, error_category)
                .tenant(Some(log_tenant.as_str()))
                .resource(Some(log_resource.as_str())),
        );

        result
    }

    /// Keyset-paginated list of `UsageRecord`s from the bound storage
    /// plugin's table, narrowed by the PDP-returned constraints.
    ///
    /// Seven responsibilities live here per
    /// `cpt-cf-usage-collector-flow-query-raw-ledger-page`:
    ///
    /// 1. **Authorize** via [`authz::authorize_list_usage_records`]. The PEP
    ///    request is pre-row — the caller has not named a specific row — so the
    ///    PDP narrows by [`AccessScope`] constraints rather than a tuple match.
    ///    It runs under `require_constraints(true)`, so a degenerate
    ///    unconstrained permit is denied in composition by
    ///    [`authz::scope_to_odata_filter`] rather than read as "all tenants".
    /// 2. **Resolve** the queried meter's declaration through
    ///    [`TypeResolver::resolve`], fail-closed, so step 3 has a declared-keys
    ///    set to check against. An unresolvable type is a pre-dispatch 404.
    /// 3. **Gate** the query surface on that declaration — the four checks of
    ///    Spec §3.11 that apply without a `group_by`
    ///    ([`reject_unpublished_filter_fields`], `reject_off_label_literals`,
    ///    `require_metadata_filter_within_caps`,
    ///    [`require_metadata_filter_keys_declared`]). The rule and its
    ///    reasoning live in [`crate::domain::query`];
    ///    [`require_dimensions_declared`] cannot run here, this path taking no
    ///    `group_by`.
    /// 4. **Compose** the PDP constraints into the caller's filter via
    ///    [`authz::scope_to_odata_filter`], intersection-only
    ///    (`composed = user_filter AND constraints`, per
    ///    `cpt-cf-usage-collector-algo-read-scope-composition`).
    ///    `gts_type_id` and `time_range` stay typed parameters and never enter
    ///    `query.filter`, which is why step 3 rejects a predicate naming a
    ///    covered-period bound rather than merging it here.
    /// 5. **Floor the keyset** — [`establish_keyset_order`] on a first page,
    ///    [`admit_continuation`] on a continuation. Here rather than in the
    ///    handler so every surface passes through it; see
    ///    [`crate::domain::query`] for the floor itself.
    /// 6. **Bind the cursor to its query** with [`read_fingerprint`], held
    ///    locally and never carried by the plugin, and enforced only here —
    ///    see [`crate::domain::query`]'s cursor lifecycle.
    /// 7. **Delegate** to the plugin's `list_usage_records` SPI with the
    ///    composed filter, floored order, bound fingerprint and typed
    ///    `time_range`, which the plugin resolves as `from <= window_end < to`
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
    ///   reserved or unpublished field, `metadata_filter` names an
    ///   undeclared metadata key, or the caller's order is not floorable
    ///   into a sound keyset (mixed sort directions, or a key that is not
    ///   a mandatory record attribute). A malformed range cannot surface
    ///   here: `time_range` arrives already validated, because
    ///   [`TimeRange`] has no public fields and `TimeRange::new` is its
    ///   only constructor.
    /// * [`UsageCollectorError::CursorRejected`] on a cursor request, when
    ///   the order the token was minted under is not one a conforming
    ///   plugin could have produced, or the token was minted over a
    ///   different query than the request carrying it — a different
    ///   `$filter`, `gts_type_id`, range or `metadata_filter`. Both carry a
    ///   wire code `toolkit_odata` owns rather than one this gear defines.
    /// * Any other [`UsageCollectorError`] variant lifted from a plugin
    ///   transport / persistence failure.
    // @cpt-flow:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-query-raw:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-tenant-isolation:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-pdp-centric-authorization:p1
    // @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p1
    // @cpt-dod:cpt-cf-usage-collector-constraint-no-business-logic:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-authorize-query:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-cross-tenant-read:p1
    //
    // Traceability (feature 2.6, usage-query): realizes
    // `flow-query-raw-ledger-page` end to end. Also realizes
    // `flow-find-withdrawal-of-record` by reuse — that flow's own step 4 is
    // "Gateway serves the read through flow-query-raw-ledger-page,
    // unchanged" — because `invalidates` (the target-reference field) is one
    // of `PUBLISHED_FILTER_FIELDS`, so a caller can narrow this same call on
    // it without any further gear code.
    // @cpt-flow:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-find-withdrawal-of-record:p1
    //
    // Traceability (feature 2.11, consistency-freshness-contract): one
    // dispatch to the bound plugin below (`instrument_spi(... plugin.
    // list_usage_records ...)`), no retry, no session/pool affinity, no
    // replication-watermark wait, and an empty page returned as an
    // ordinary success rather than an error — the raw-list half of
    // `algo-staleness-defensive-read` and of
    // `dod-consistency-no-stronger-read-claim`.
    // @cpt-dod:cpt-cf-usage-collector-dod-consistency-no-stronger-read-claim:p1
    pub async fn list_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError> {
        let start = std::time::Instant::now();
        // Captured before `gts_type_id` moves into the `async move` block
        // below: this operation's log entry is emitted after the block
        // returns.
        let log_referenced_type = gts_type_id.as_str().to_owned();
        let result = async move {
            // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-scope
            // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-attribution
            // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-denied
            let scope = authz::authorize_list_usage_records(
                &self.enforcer,
                self.metrics.as_ref(),
                PdpOp::QueryRaw,
                ctx,
            )
            .await
            .map_err(UsageCollectorError::from)?;
            // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-denied
            // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-attribution
            // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-scope

            // Authorization composed → track in-flight for the remainder of
            // this attempt (decremented on every exit below, `?` included).
            let _inflight = QueryInflightGuard::enter(self.metrics.as_ref(), QueryKind::Raw);

            // Resolve the queried meter's declaration so the admissibility
            // gate below has a declared-keys set to check `metadata_filter`
            // against. An unresolvable type fails closed as a pre-dispatch
            // 404, never reaching the plugin.
            // @cpt-flow:cpt-cf-usage-collector-flow-resolve-for-read:p1
            let declaration = self.type_resolver.resolve(&gts_type_id).await?;

            // The Spec §3.11 query-surface gate, recomputed per request so a
            // property declared a moment ago is usable on this very call. The
            // rule and why each check exists: [`crate::domain::query`].
            // `require_dimensions_declared` does not apply on this path, which
            // takes no `group_by`.
            if let Some(filter) = query.filter() {
                reject_unpublished_filter_fields(filter)?;
                reject_off_label_literals(filter)?;
            }
            require_metadata_filter_within_caps(metadata_filter)?;
            require_metadata_filter_keys_declared(
                metadata_filter,
                declaration.metadata_schema.declared_keys(),
                &gts_type_id,
            )?;

            // The query a keyset continuation is bound to: the CALLER's
            // `$filter` plus every typed parameter, computed before
            // composition AND-merges the server-injected PDP scope in (a
            // value the next request could never reproduce). Behind the
            // service rather than at the REST edge so every surface has one
            // owner — see [`crate::domain::query`]'s cursor lifecycle.
            let fingerprint = read_fingerprint(&gts_type_id, time_range, query, metadata_filter);

            // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-scope
            let mut composed = compose_query_with_scope(query, &scope)?;
            // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-scope

            // The keyset floor ([`crate::domain::query`]), applied here
            // because this is the one place every caller passes through. The
            // branch is in the open because the two modes are different
            // operations: a first page's order is normalized (idempotently,
            // so the REST handler having done it makes this a no-op), while a
            // continuation's came from its token and is only checked — the
            // only place a cursor-reconstructed order is checked at all.
            // Composition above only rewrites `filter`, so the order reaching
            // here is unchanged.
            match composed.cursor {
                // The fingerprint is passed in rather than read off
                // `composed.filter_hash`, which holds the caller's own
                // filter-only hash: reading it back would refuse every
                // legitimate page two.
                Some(_) => admit_continuation(&mut composed, &fingerprint)?,
                None => establish_keyset_order(&mut composed)?,
            }

            // The continuation, decoded. `admit_continuation` above has
            // already bound `composed.order` from the token and checked it
            // is a sound keyset, so this reads the boundary values against
            // an order that is already the right one.
            let keyset = match composed.cursor.as_ref() {
                Some(cursor) => Some(query::keyset_from_cursor(cursor, &composed.order)?),
                None => None,
            };

            // The SPI states two MUST-NOTs on this dispatch: `query.cursor`
            // and `query.filter_hash` must not be read. Both are stripped
            // here rather than left for a plugin to ignore, so the guarantee
            // is structural. `composed.filter_hash` is not naturally `None` —
            // composition preserves the caller's filter-only hash — so
            // omitting this would hand the plugin a stale value.
            composed.cursor = None;
            composed.filter_hash = None;

            let plugin = self.resolve_plugin_for(PluginOp::ListUsageRecords).await?;

            // The reference the plugin keys storage on, taken from the
            // declaration this call already resolved above — never derived
            // here (`MeterRef`'s own doc).
            let meter = MeterRef::new(declaration.type_uuid, gts_type_id.clone());

            // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-dispatch
            // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-plugin-catch
            let page = instrument_spi(
                self.metrics.as_ref(),
                PluginOp::ListUsageRecords,
                plugin.list_usage_records(
                    &meter,
                    time_range,
                    &composed,
                    metadata_filter,
                    keyset.as_ref(),
                ),
            )
            .await
            // `algo-plugin-dispatch` step 8's "CATCH ... and classify it with
            // algo-plugin-error-classification": both the `impl From` and
            // `lift_domain_error` are marked in `domain/error.rs`.
            // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-catch
            .map_err(|e| {
                lift_domain_error(
                    DomainError::from(e),
                    UnavailableRetryAfterSecs(self.unavailable_retry_after_secs),
                    TargetNotConvergedRetryAfterSecs(self.target_not_converged_retry_after_secs),
                )
            })?;
            // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-catch
            // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-plugin-catch
            // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-dispatch

            // The gear mints from whatever the plugin returned, so this is
            // the last chance to catch a malformed keyset before it becomes
            // a token a caller follows: wrong arity, a direction that
            // disagrees with the dispatched order, or a continuation on a
            // page with no rows.
            query::verify_returned_keyset(&composed.order, &page)?;

            // The gateway mints the token from the plugin's returned
            // keyset, bound to the order it dispatched under
            // (`cpt-cf-usage-collector-dod-gateway-owned-cursor`).
            let next_cursor = page
                .next
                .as_ref()
                .map(|ks| query::mint_record_cursor(&composed.order, ks, &fingerprint))
                .map(|cursor| cursor.encode())
                .transpose()
                .map_err(|err| {
                    UsageCollectorError::internal(format!(
                        "usage-collector could not encode the raw page's continuation: {err}"
                    ))
                })?;

            // `PageInfo.limit` is on the wire and the plugin reports none, so
            // it is resolved here — see [`DEFAULT_PAGE_SIZE`] for why the
            // default and the clamp are both load-bearing.
            let limit = composed
                .limit
                .unwrap_or(DEFAULT_PAGE_SIZE)
                .clamp(1, MAX_PAGE_SIZE);

            // The identifier is stapled back on per entry, and the
            // reference each entry carries is checked against the one this
            // read dispatched before it is: a row under another meter's
            // reference would otherwise be relabelled with the queried
            // meter's identifier and served to a caller as fact.
            let mut items = Vec::with_capacity(page.items.len());
            for stored in page.items {
                items.push(reattach(stored, &meter)?);
            }

            // `items` flows through untouched otherwise — no fold, no marker,
            // no suppression, no reordering — realizing
            // `algo-raw-ledger-projection` / `dod-raw-returns-persisted-pair`.
            // `ODataPage` is the platform's canonical page envelope, defining
            // no paging schema of its own — the raw-path half of
            // `dod-canonical-page-envelope`.
            // @cpt-algo:cpt-cf-usage-collector-algo-raw-ledger-projection:p1
            // @cpt-dod:cpt-cf-usage-collector-dod-raw-returns-persisted-pair:p1
            // @cpt-dod:cpt-cf-usage-collector-dod-canonical-page-envelope:p1
            Ok(ODataPage::new(
                items,
                PageInfo {
                    next_cursor,
                    // The raw path reads forward only
                    // (`usage-collector-v1.yaml`, the `Cursor` parameter).
                    prev_cursor: None,
                    limit,
                },
            ))
        }
        .await;
        let seconds = start.elapsed().as_secs_f64();
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-result-rows-observe
        if let Ok(page) = &result {
            self.metrics.observe_query_result_rows(
                QueryKind::Raw,
                u64::try_from(page.items.len()).unwrap_or(u64::MAX),
            );
        }
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-result-rows-observe
        // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-telemetry-complete
        let (outcome, error_category) = classify_query_result(&result);
        self.metrics
            .record_query_request(QueryKind::Raw, outcome, error_category, seconds);
        let log_tenant = ctx.subject_tenant_id().to_string();
        log_operation_completed(
            OperationLogEntry::new(PdpOp::QueryRaw, outcome, error_category)
                .tenant(Some(log_tenant.as_str()))
                .referenced_type(Some(log_referenced_type.as_str())),
        );
        // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-telemetry-complete
        result
    }

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
    ///    [`TypeResolver::resolve`], fail-closed. There is no caller-chosen
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
        // Captured before `gts_type_id` moves into the `async move` block
        // below, same as the raw-list sibling above.
        let log_referenced_type = gts_type_id.as_str().to_owned();
        let result = async move {
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
            let _inflight = QueryInflightGuard::enter(self.metrics.as_ref(), QueryKind::Aggregated);

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
            instrument_spi(
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
        let seconds = start.elapsed().as_secs_f64();
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-result-rows-observe
        if let Ok(aggregation_result) = &result {
            self.metrics.observe_query_result_rows(
                QueryKind::Aggregated,
                u64::try_from(aggregation_result.buckets.len()).unwrap_or(u64::MAX),
            );
        }
        // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-result-rows-observe
        // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-telemetry-complete
        let (outcome, error_category) = classify_query_result(&result);
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
    /// disagrees with the declared fold
    /// ([`reconciliation::check_summary_branch`]).
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
        let start = std::time::Instant::now();
        // Captured before `gts_type_id` moves into the `async move` block
        // below. `tenant_id` is `Copy` and needs no such capture.
        let log_referenced_type = gts_type_id.as_str().to_owned();
        let result = async move {
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

            let plugin = self
                .resolve_plugin_for(PluginOp::GetReconciliationMetadata)
                .await?;

            // The reference the plugin keys storage on, taken from the
            // declaration this call already resolved above — never derived
            // here (`MeterRef`'s own doc).
            let meter = MeterRef::new(declaration.type_uuid, gts_type_id.clone());

            let metadata = instrument_spi(
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

            reconciliation::check_summary_branch(fold, &metadata.quantity_summary)?;

            Ok(metadata)
        }
        .await;

        let seconds = start.elapsed().as_secs_f64();
        let (outcome, error_category) = classify_query_result(&result);
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

    /// Replay-safe feed page in feed order (DESIGN §3.1).
    ///
    /// Realises `cpt-cf-usage-collector-component-feed-gateway` and the
    /// `cpt-cf-usage-collector-seq-read-feed` sequence.
    ///
    /// Authorization runs **before** the cursor is examined, so a caller
    /// holding no permit receives `PermissionDenied` rather than a diagnosis
    /// of their token: decoding first would tell an unauthorized caller
    /// whether their token was well-formed, bound to this subscription and
    /// still resumable — three facts about a stream they may not read at all.
    ///
    /// No age is computed anywhere on this path: ADR-0011's two zones are
    /// zones of published guarantee, not branches, and the refusal is the
    /// plugin's, keyed on a retention mark. `feed_tests.rs`'s
    /// age-absence pin scans this function's body for exactly that.
    ///
    /// The `until` bound is **passed through, never compared** against the
    /// resume cursor. A [`usage_collector_sdk::FeedPosition`] is opaque bytes
    /// here, so the gateway has no ordering to compare them under; an `until`
    /// at or behind the cursor reaches the plugin, which answers an empty
    /// page.
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::PermissionDenied`] /
    ///   [`UsageCollectorError::ServiceUnavailable`] when the PDP denies, is
    ///   unavailable, or returns a constraint shape this gear cannot project.
    /// * [`UsageCollectorError::CursorRejected`] when `start`'s cursor or
    ///   `until` is not a feed cursor for this subscription, on the field it
    ///   arrived as.
    /// * [`UsageCollectorError::InvalidArgument`] for a `limit` outside the
    ///   published bound, and — carrying
    ///   [`ValidationReason::CursorBeyondRetention`] — for the plugin's
    ///   replay refusal.
    /// * [`UsageCollectorError::NotFound`] when `subscription` names a meter
    ///   with no resolvable declaration. Resolution runs per subscribed
    ///   type and is all-or-nothing ([`Self::resolve_subscription_meters`]):
    ///   one unresolvable meter fails the whole read rather than serving a
    ///   page silently missing that meter's entries.
    /// * [`UsageCollectorError::ServiceUnavailable`] when a subscribed
    ///   meter's declaration cannot be resolved because `types-registry` is
    ///   unreachable and nothing is cached.
    /// * Any other [`UsageCollectorError`] variant lifted from a plugin
    ///   transport / persistence failure.
    // @cpt-flow:cpt-cf-usage-collector-component-feed-gateway:p1
    // @cpt-flow:cpt-cf-usage-collector-seq-read-feed:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-first-connect:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-resume-after-outage:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-bounded-replay:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-observe-correction:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-feed-recover-refused-cursor:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-feed-request-admission:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-feed-page-assembly:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-feed-retention-refusal:p1
    // @cpt-state:cpt-cf-usage-collector-state-feed-cursor-servability:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-next-cursor-always:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-corrections-as-entries:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-retention-refusal:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-at-least-once:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-feed-unstripped-entries:p1
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    pub async fn read_usage_feed(
        &self,
        ctx: &SecurityContext,
        subscription: &FeedSubscription,
        start: FeedStart<&CursorV1>,
        until: Option<&CursorV1>,
        limit: Option<u64>,
    ) -> Result<FeedPage<CursorV1>, UsageCollectorError> {
        let started = std::time::Instant::now();
        let result = async move {
            // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-authorize
            let scope = authz::authorize_read_usage_feed(
                &self.enforcer,
                self.metrics.as_ref(),
                PdpOp::ReadFeed,
                ctx,
            )
            .await
            .map_err(UsageCollectorError::from)?;
            // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-authorize

            // No [`QueryInflightGuard`] here, and that is a decision rather
            // than an omission: `uc_query_inflight` is labelled by
            // [`QueryKind`], whose published value set is `aggregated` / `raw`
            // — entering the feed under either would mis-attribute it, and
            // DESIGN §3.11.5 declares no feed gauge to widen it for. This path
            // feeds the feed's own three instruments instead.
            let limit = feed::resolve_limit(limit)?;
            // Beside `resolve_limit` and for the same reason: the REST edge
            // is not the only caller. See the helper for why this is a
            // second check rather than a moved one.
            feed::require_subscription_breadth(subscription)?;

            // Resolved after the breadth cap and before the plugin is
            // resolved: the cap bounds how many resolves one request can
            // provoke, and a subscription naming an undeclared meter fails
            // closed here rather than reaching the plugin at all.
            let meters = self.resolve_subscription_meters(subscription).await?;

            let spi_start = match start {
                FeedStart::Oldest => FeedStart::Oldest,
                FeedStart::After(cursor) => FeedStart::After(feed::position_from_cursor(
                    cursor,
                    subscription,
                    CursorField::Cursor,
                )?),
                // `FeedStart` is `#[non_exhaustive]` and foreign, so this arm
                // is required — and required to be a refusal rather than a
                // fallthrough to `Oldest`: a start mode admitted later and
                // silently reinterpreted as "from the oldest retained entry"
                // would re-serve a consumer's whole retained history as if it
                // were new, which on this path is a re-billed invoice.
                ref other => {
                    return Err(UsageCollectorError::internal(format!(
                        "unrecognised feed start mode: {other:?}"
                    )));
                }
            };

            // `until` is decoded on its own field name, so a consumer who
            // pasted a stale bound is told which parameter is at fault
            // — but it is never compared with `spi_start`.
            let spi_until = until
                .map(|cursor| feed::position_from_cursor(cursor, subscription, CursorField::Until))
                .transpose()?;

            // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-dispatch
            let plugin = self.resolve_plugin_for(PluginOp::ReadFeedPage).await?;

            let page = instrument_spi(
                self.metrics.as_ref(),
                PluginOp::ReadFeedPage,
                plugin.read_feed_page(&meters, &scope, spi_start, spi_until, limit),
            )
            .await
            .map_err(|e| {
                lift_feed_dispatch_error(
                    e,
                    &start,
                    self.unavailable_retry_after_secs,
                    self.target_not_converged_retry_after_secs,
                )
            })?;
            // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-dispatch

            // A page longer than the plugin was told to serve is refused, not
            // served and not truncated. The defect is in the page itself
            // rather than in the token that continues it, so unlike an absent
            // continuation it cannot be diagnosed and served: the alternative
            // is handing a charging consumer a body its own schema validator
            // rejects with nothing naming the component at fault. Truncating
            // would drop entries the `next` position already sits past — a
            // gap in an invoice rather than a duplicate.
            //
            // `internal`, not the aggregate path's `InvalidArgument`: there
            // the cap is reachable by a caller widening `group_by`, while here
            // `limit` is the caller's own number and the plugin exceeded it.
            // A host-contract breach, counted under `plugin_error`.
            if page.entries.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
                return Err(UsageCollectorError::internal(format!(
                    "usage-collector storage plugin served {} feed entries for a page limit of \
                     {limit}; a page longer than the limit it was dispatched under cannot be \
                     handed to a consumer",
                    page.entries.len(),
                )));
            }

            // Diagnosed, not decided — see the helper for why the feed's
            // half of this has no refusal available to it.
            report_absent_live_continuation(&page, until.is_some());

            // @cpt-begin:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-map-result
            // Mapped, never decided. DESIGN §3.2 fixes both dispositions on
            // the PLUGIN — `Some` on every page of a live read, short pages
            // included, `None` once a bounded replay has reached its `until`
            // — and a gateway-side branch would be a second, disagreeing
            // authority whose first disagreement hands a live consumer a
            // `None` that reads as "this stream has ended". Hence the breach
            // is reported above rather than left to the `map` below.
            //
            // The identifier is re-attached per entry, from the subscription
            // this read resolved; an entry under a reference the subscription
            // never named has no identifier to re-attach at all — a typed
            // breach, not an index into a missing key.
            //
            // Built with an explicit `insert` rather than `.collect()`, which
            // would silently keep whichever identifier landed last on a
            // duplicate `uuid`. The one-identifier-to-one-reference guarantee
            // (`cpt-cf-types-registry-adr-storage-identity-query-model`) makes
            // that unreachable, but this is where it is load-bearing: were it
            // violated, every entry of both meters would be relabelled under a
            // meter that is not its own — the one route the re-attachment
            // guards elsewhere do not close. A repeated meter in one
            // subscription is harmless.
            let mut by_reference: HashMap<Uuid, &MeterTypeId> = HashMap::new();
            for m in &meters {
                if let Some(existing) = by_reference.insert(m.uuid, &m.id)
                    && existing != &m.id
                {
                    return Err(invariant_breach(format!(
                        "subscription resolved registry reference {} to two distinct \
                         identifiers, {existing} and {}; one identifier must map to one \
                         reference for the life of an installation",
                        m.uuid, m.id,
                    )));
                }
            }
            let entries = page
                .entries
                .into_iter()
                .map(|stored| {
                    let gts_type_id = by_reference.get(&stored.gts_type_uuid).ok_or_else(|| {
                        invariant_breach(format!(
                            "storage plugin answered a feed page with an entry under registry \
                             reference {}, which this subscription does not name",
                            stored.gts_type_uuid,
                        ))
                    })?;
                    Ok(stored.into_usage_record((*gts_type_id).clone()))
                })
                .collect::<Result<Vec<_>, UsageCollectorError>>()?;

            Ok(FeedPage {
                entries,
                next: page.next.map(|p| feed::mint_cursor(&p, subscription)),
            })
            // @cpt-end:cpt-cf-usage-collector-seq-read-feed:p1:inst-feed-map-result
        }
        .await;

        let seconds = started.elapsed().as_secs_f64();
        if let Ok(page) = &result {
            self.metrics
                .observe_feed_page_entries(u64::try_from(page.entries.len()).unwrap_or(u64::MAX));
        }
        let (outcome, error_category) = classify_feed_result(&result);
        self.metrics
            .record_feed_request(outcome, error_category, seconds);
        // No `resource` / `referenced_type`: a feed page is read over a
        // subscription naming potentially several `MeterTypeId`s
        // (`cpt-cf-usage-collector-fr-...-feed-subscription`), so there is no
        // single value to carry (`inst-log-identifiers-here` carries those
        // only where the operation has exactly one).
        let log_tenant = ctx.subject_tenant_id().to_string();
        log_operation_completed(
            OperationLogEntry::new(PdpOp::ReadFeed, outcome, error_category)
                .tenant(Some(log_tenant.as_str())),
        );
        result
    }

    /// Resolve every subscribed meter to the reference the Plugin SPI takes.
    ///
    /// **All or nothing.** A subscription is read as one page across all its
    /// meters, so dispatching the subset that resolved would serve a page
    /// silently missing a subscribed meter's entries — a consumer could not
    /// tell that from the meter having had no traffic. The first failure
    /// therefore fails the read, which is also what the three parameter-carrying
    /// read paths already do for a single meter.
    ///
    /// Resolution is sequential rather than fanned out: the resolver's cache
    /// makes a warm subscription free, and a cold one is bounded by the
    /// published subscription-breadth cap (`feed::require_subscription_breadth`,
    /// checked before this runs).
    #[allow(
        clippy::result_large_err,
        reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
    )]
    async fn resolve_subscription_meters(
        &self,
        subscription: &FeedSubscription,
    ) -> Result<Vec<MeterRef>, UsageCollectorError> {
        let mut meters = Vec::with_capacity(subscription.types().len());
        for gts_type_id in subscription.types() {
            let declaration = self.type_resolver.resolve(gts_type_id).await?;
            meters.push(MeterRef::new(declaration.type_uuid, gts_type_id.clone()));
        }
        Ok(meters)
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
    // @cpt-flow:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-plugin-lazy-binding:p1
    // @cpt-dod:cpt-cf-usage-collector-fr-pluggable-storage:p1
    // @cpt-dod:cpt-cf-usage-collector-nfr-availability:p2
    // @cpt-dod:cpt-cf-usage-collector-principle-pluggable-storage:p2
    // @cpt-dod:cpt-cf-usage-collector-principle-plugin-resolution-via-client-hub:p2
    // @cpt-dod:cpt-cf-usage-collector-principle-fail-closed:p2
    // @cpt-dod:cpt-cf-usage-collector-adr-pluggable-storage:p2
    // @cpt-dod:cpt-cf-usage-collector-constraint-vendor-pluggable:p2
    // @cpt-dod:cpt-cf-usage-collector-constraint-plugin-contract-stability:p2
    // @cpt-dod:cpt-cf-usage-collector-constraint-nfr-thresholds:p2
    // @cpt-dod:cpt-cf-usage-collector-contract-storage-plugin:p1
    // @cpt-dod:cpt-cf-usage-collector-contract-gts-registry:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-readiness-signal-derivation:p2
    // @cpt-state:cpt-cf-usage-collector-state-readiness-signal:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-readiness-signals:p2
    pub async fn get_plugin(&self) -> Result<Arc<dyn UsageCollectorPluginV1>, DomainError> {
        // `get_or_init` alone realizes both the cache-hit check (step 1) and
        // the selector query (step 2) of `algo-plugin-binding-resolution`;
        // `OnceCell` branches between them invisibly, so neither step is
        // distinctly bracketable at this call site.
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-enter-selector
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-first-dispatch
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-cold-path
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-cache-instance
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-cache-instance
        let instance_id = match self.selector.get_or_init(|| self.resolve_plugin()).await {
            Ok(instance_id) => instance_id,
            // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-cache-instance
            // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-cache-instance
            // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-no-instance
            Err(e) => {
                // Selector resolution failed — structural readiness fact does
                // not hold.
                // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-readiness-fact
                self.metrics.set_plugin_ready(false);
                // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-readiness-fact
                tracing::warn!(
                    error = %e,
                    vendor = %self.vendor,
                    "usage-collector plugin selector resolution failed"
                );
                return Err(e);
            } // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-no-instance
        };
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-cold-path
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-first-dispatch
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-enter-selector

        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-try-get-scoped
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-scoped-lookup
        let scope = ClientScope::gts_id(instance_id.as_ref());
        let client = self
            .hub
            .try_get_scoped::<dyn UsageCollectorPluginV1>(&scope);
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-scoped-lookup
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-try-get-scoped

        // Flow step 8's own RETURN is the running gear `Gear::init` hands
        // back (`module.rs`'s `inst-vendor-return`), not this per-call client
        // handle — this branch stays on the flow's pre-rebind residue name.
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-return-handle
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-return
        if let Some(client) = client {
            // Structural readiness fact holds: selector cached an instance id
            // AND the scoped client is registered.
            // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-readiness-fact
            // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-readiness-fact
            self.metrics.set_plugin_ready(true);
            // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-readiness-fact
            // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-readiness-fact
            return Ok(client);
        }
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-return
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-return-handle

        // Scoped client not registered — structural readiness fact does not
        // hold. No declared step of `flow-plugin-vendor-selection` names a
        // `ClientHub` lookup miss specifically, so this branch stays on the
        // flow's pre-rebind instance name.
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-return-handle
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-lookup-miss
        // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-readiness-fact
        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-readiness-fact
        self.metrics.set_plugin_ready(false);
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-algo-binding-readiness-fact
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-readiness-fact
        tracing::warn!(
            plugin_gts_id = %instance_id,
            vendor = %self.vendor,
            "usage-collector storage plugin client not registered yet"
        );
        Err(DomainError::PluginUnavailable {
            gts_id: Some(instance_id.to_string()),
            reason: "client not registered yet".into(),
        })
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-lookup-miss
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-return-handle
    }

    /// Resolve the bound storage-plugin handle for an SPI dispatch, recording
    /// `uc_plugin_accept_errors_total{operation, error_category="unready"}`
    /// when the structural binding is unavailable (no SPI dispatch occurred,
    /// so no duration is recorded). `op` labels the SPI method that would have
    /// been dispatched; when a method issues several SPI calls, the first is
    /// used (they share one handle, so a resolution failure aborts them all).
    /// The `uc_plugin_ready` gauge is maintained by [`Self::get_plugin`].
    ///
    /// Returns [`UsageCollectorError`] directly, not [`DomainError`]: this is
    /// the chokepoint every call site shares (`get_plugin` is called from
    /// nowhere else), so it is also where `unavailable_retry_after_secs`
    /// (DESIGN §3.8) is applied to [`Self::get_plugin`]'s three
    /// `ServiceUnavailable`-shaped outcomes via [`lift_domain_error`].
    /// `InvalidPluginInstance` is excluded by construction, taking
    /// `lift_domain_error`'s unmodified `other` arm and keeping its `Internal`
    /// (500) classification. Fail-closed with no substituted binding
    /// (`cpt-cf-usage-collector-dod-plugin-fail-closed`): no fallback and no
    /// retained stale client — `get_plugin` re-reads `try_get_scoped` on every
    /// call, so a later call succeeds as soon as a plugin is reachable again.
    // @cpt-algo:cpt-cf-usage-collector-algo-plugin-dispatch:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-plugin-fail-closed:p1
    // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-resolve
    async fn resolve_plugin_for(
        &self,
        op: PluginOp,
    ) -> Result<Arc<dyn UsageCollectorPluginV1>, UsageCollectorError> {
        match self.get_plugin().await {
            Ok(plugin) => Ok(plugin),
            // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-unready
            // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-unready-counter
            Err(e) => {
                self.metrics
                    .record_plugin_accept_error(op, PluginErrorCategory::Unready);
                Err(lift_domain_error(
                    e,
                    UnavailableRetryAfterSecs(self.unavailable_retry_after_secs),
                    TargetNotConvergedRetryAfterSecs(self.target_not_converged_retry_after_secs),
                ))
            } // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-unready-counter
              // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-algo-plugin-dispatch-unready
        }
    }
    // @cpt-end:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-resolve

    /// Resolves the bound storage-plugin instance id from `types-registry`:
    /// `algo-plugin-binding-resolution` step 2's selector query — matching
    /// exactly on `UsageCollectorPluginSpecV1`'s schema identifier via
    /// `list_instances`'s pattern — and encloses step 4's lowest-priority
    /// selection too, which `choose_plugin_instance` (`toolkit::plugins`)
    /// performs; the delegation below is that step's realization.
    // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-first-dispatch
    // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-selector-query
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

        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-lowest-priority
        let gts_id = choose_plugin_instance::<UsageCollectorPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )?;
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-lowest-priority

        info!(plugin_gts_id = %gts_id, "Selected usage-collector storage plugin instance");

        Ok(gts_id)
    }
    // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-selector-query
    // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-first-dispatch

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
pub(crate) mod service_metrics_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_reference_tests.rs"]
mod service_reference_tests;
