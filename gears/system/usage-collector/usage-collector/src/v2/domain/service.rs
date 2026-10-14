//! Domain service for the usage-collector module.
//!
//! `Service` owns the lazy storage-plugin binding: resolved on first
//! dispatch via `GtsPluginSelector` (single-flight `get_or_init`) and
//! cached for the service's lifetime, so a binding change needs a module
//! restart. Readiness is computed per dispatch since the SPI has no
//! plugin-side `ready()` probe.
//!
//! Authorization is a direct PDP call per operation through
//! [`crate::v2::domain::authz`], which owns the resource/action vocabularies.

use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::PolicyEnforcer;
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use tracing::info;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient, TypesRegistryError};
use usage_collector_sdk::v2::{
    CreateUsageRecord, EntryType, IngestionQuotaExceededArgs, RecordOrigin, UsageCollectorError,
    UsageCollectorPluginSpecV2, UsageCollectorPluginV2, UsageRecord,
};
use uuid::Uuid;

use crate::config::IngestionQuotaConfig;
use crate::v2::domain::authz;
use crate::v2::domain::covered_period::CoveredPeriodBounds;
use crate::v2::domain::ingestion;
use crate::v2::domain::meter_reverse::MeterReverseResolver;
use crate::v2::domain::observability;
use crate::v2::domain::observability::{OperationLogEntry, log_operation_completed};
use crate::v2::domain::ports::declaration_mirror::NoopDeclarationMirror;
use crate::v2::domain::ports::declarations::UnavailableDeclarationSource;
use crate::v2::domain::ports::metrics::{
    IngestRequestErrorCategory, IngestRequestOutcome, NoopMetrics, PdpOp, PluginErrorCategory,
    PluginOp, QueryErrorCategory, QueryKind, RecordErrorCategory, RecordOutcome, RequestOutcome,
    UsageCollectorMetrics,
};
use crate::v2::domain::quota::IngestionQuota;
use crate::v2::domain::type_resolver::{TypeResolver, TypeResolverConfig};
use crate::v2::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES;

use super::error;
use super::error::{
    DomainError, TargetNotConvergedRetryAfterSecs, UnavailableRetryAfterSecs, lift_domain_error,
};

/// Default per-request entry cap, used when `[usage_collector].max_batch_records`
/// is not configured. The cap is operator configuration, so the wire schema
/// publishes no `maxItems`; it bounds what one caller sends, not what one
/// backend write holds.
pub const DEFAULT_MAX_BATCH_RECORDS: usize = 100;

/// Default `[usage_collector].unavailable_retry_after_secs`, used when the
/// key is not configured: a `ServiceUnavailable` that reached no retry hint of
/// its own (a hintless plugin `Transient`, or a dispatch that reached no plugin
/// at all) carries this delay rather than none.
pub const DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS: u64 = 1;

/// Default `[usage_collector].target_not_converged_retry_after_secs`, used
/// when the key is not configured: a `Conflict(TargetNotConverged)` carries
/// this delay.
pub const DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS: u64 = 1;

/// The `PageInfo.limit` reported on a raw-path page when `query.limit` is
/// absent.
///
/// REST's `prepare_list_query` always populates `query.limit` before
/// dispatch, so this default is reached only by a bare in-process caller.
/// Mirrors the `TimescaleDB` plugin's own private `DEFAULT_PAGE_SIZE` so the
/// reported figure matches what that backend paginates by when it is not told
/// otherwise.
///
/// Always used together with [`MAX_PAGE_SIZE`] as a clamp, never alone: an
/// in-process caller's `limit` does not pass REST's `$top` cap, so this default
/// alone would report a `PageInfo.limit` above what every plugin still serves.
/// `local_client.rs` forwards the page verbatim, so a wrong figure here reaches
/// a calling gear as fact.
pub(crate) const DEFAULT_PAGE_SIZE: u64 = 100;

/// Maximum number of records a raw-path page may report as its
/// `PageInfo.limit`, and the REST `$top` / `limit` ceiling a caller-supplied
/// value above it is rejected against.
///
/// Domain-owned: the REST handler (`api::rest::handlers::usage_records`)
/// re-exports this rather than defining its own, so the wire-edge cap and the
/// domain-level clamp are one constant rather than two that could drift apart.
pub(crate) const MAX_PAGE_SIZE: u64 = 1000;

/// `usage-collector` domain service.
///
/// Discovers the bound storage plugin via `types-registry` and delegates
/// the entry ledger to it; owns the lazy binding resolution.
///
/// The one exception is the Type Resolver's declaration mirror, a durable
/// table this gear owns directly over `toolkit-db` rather than through the
/// Plugin SPI.
///
/// The single Ingestion Gateway component: every submission funnels through
/// `create_usage_records*` / `backfill_usage_records*` into
/// `crate::v2::domain::ingestion::run_ingestion_pipeline`, the only write path.
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

    /// PEP boundary. The PDP is a hard dependency: the host fails init if no
    /// resolver client is registered, so this field is always populated at
    /// runtime.
    pub(super) enforcer: PolicyEnforcer,

    /// Operational-metrics sink. Injected at gear bootstrap via
    /// [`Service::new_with_metrics`]; [`Service::new`] defaults it to a
    /// no-op adapter for tests and pre-init contexts. The concrete
    /// OTLP-backed adapter lives in [`crate::v2::infra::metrics`]; the domain
    /// depends only on the [`UsageCollectorMetrics`] port.
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-observability-plugin-host-instruments:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-foundation-observability-pdp-helper-instruments:p1
    pub(super) metrics: Arc<dyn UsageCollectorMetrics>,

    /// Resolves a meter's GTS type declaration (fold, canonical unit,
    /// metadata surface) through a local TTL cache in front of
    /// `types-registry`, so ingestion's own NFRs stay independent of a
    /// second gear's availability and latency — see
    /// [`crate::v2::domain::type_resolver`].
    pub(super) type_resolver: Arc<TypeResolver>,

    /// The forward resolver's counterpart: resolves a registry reference
    /// back to the meter identifier it was issued for.
    ///
    /// Consulted only by [`Self::get_usage_record`], which takes no meter
    /// and so has no identifier in scope to re-attach directly. Shares its
    /// adapter/mirror with `type_resolver`
    /// ([`crate::v2::infra::types_registry_source::build_default_resolvers`]).
    reverse_resolver: Arc<MeterReverseResolver>,

    /// Cap on an entry's serialized metadata map, in bytes, enforced by
    /// [`validate_submit_record_metadata`](crate::v2::domain::validation::validate_submit_record_metadata) on every ingestion path.
    /// [`Self::new_with_metrics`] takes it as a mandatory `usize`;
    /// [`Self::new`] is the one place [`DEFAULT_METADATA_SIZE_CAP_BYTES`]
    /// applies, production bootstrap threading the configured value instead.
    pub(super) metadata_size_cap_bytes: usize,

    /// The covered-period bounds both ingestion paths enforce, projected
    /// from the configured `[usage_collector]` block by
    /// `UsageCollectorConfig::covered_period_bounds`. Held as the finished
    /// [`CoveredPeriodBounds`] rather than as the whole config, for the
    /// same reason `metadata_size_cap_bytes` is held as a plain `usize`.
    pub(super) covered_period_bounds: CoveredPeriodBounds,

    /// The per-request entry cap both batch ingestion routes enforce
    /// (`create_usage_records_for_origin`). Defaults to
    /// [`DEFAULT_MAX_BATCH_RECORDS`] via [`Self::new`]; production bootstrap
    /// (`module.rs`) instead threads `UsageCollectorConfig::max_batch_records`
    /// through explicitly, mirroring `metadata_size_cap_bytes`.
    max_batch_records: usize,

    /// Per-subject ingestion token buckets. One set for the whole service —
    /// no per-route/origin bucket, workload isolation rather than a separate
    /// budget being what distinguishes the routes.
    ///
    /// The anonymous caller is a key like any other:
    /// `SecurityContext::anonymous()`'s nil `subject_id` means every
    /// anonymous caller shares one bucket, bounding anonymous traffic in
    /// aggregate rather than exempting it.
    quota: IngestionQuota,

    /// `[usage_collector].unavailable_retry_after_secs`: the retry delay a
    /// `ServiceUnavailable` carries where nothing else supplied one, applied
    /// by [`lift_domain_error`] at every plugin-dispatch and plugin-host lift
    /// site. Defaults to [`DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS`] via
    /// [`Self::new`], as `max_batch_records` does.
    pub(super) unavailable_retry_after_secs: u64,

    /// `[usage_collector].target_not_converged_retry_after_secs`: the delay a
    /// `Conflict(TargetNotConverged)` carries, applied by
    /// [`lift_domain_error`] at every site that can raise one. Defaults to
    /// [`DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS`] via [`Self::new`].
    pub(super) target_not_converged_retry_after_secs: u64,
}

/// The operator-configured knobs [`Service`] enforces, projected from the
/// `[usage_collector]` block by
/// [`UsageCollectorConfig::service_config`](crate::config::UsageCollectorConfig::service_config).
///
/// One struct rather than six constructor arguments: these values share a
/// single origin (one config block, validated as a unit by
/// `UsageCollectorConfig::validate`) and a single lifetime (read once at
/// `Gear::init`, fixed for the service's lifetime). [`Service`] holds them as
/// flat fields, the values being read individually across the `v2` domain.
///
/// [`Default`] reproduces the published defaults, and agrees with
/// `UsageCollectorConfig::default().service_config()` by construction — both
/// cite the same constants.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Cap on an entry's serialized metadata map, in bytes, enforced on
    /// every ingestion path by
    /// [`validate_submit_record_metadata`](crate::v2::domain::validation::validate_submit_record_metadata).
    pub metadata_size_cap_bytes: usize,

    /// The covered-period bounds both ingestion paths enforce. Held as the
    /// finished [`CoveredPeriodBounds`] rather than as the raw seconds, so
    /// the fallible-looking `i64` projection stays at the config edge.
    pub covered_period_bounds: CoveredPeriodBounds,

    /// The per-request entry cap both batch ingestion routes enforce
    /// (`create_usage_records_for_origin`).
    pub max_batch_records: usize,

    /// Configures the service's per-subject ingestion token buckets. Taken as
    /// the config rather than as a built [`IngestionQuota`]: the buckets are
    /// service-owned mutable state, so the service builds them and no caller
    /// can hold a second handle to them.
    pub ingestion_quota: IngestionQuotaConfig,

    /// The retry delay a `ServiceUnavailable` carries where nothing else
    /// supplied one, applied by `lift_domain_error` at every
    /// plugin-dispatch and plugin-host lift site.
    pub unavailable_retry_after_secs: u64,

    /// The retry delay a `Conflict(TargetNotConverged)` carries, applied by
    /// `lift_domain_error` at every site that can raise one.
    pub target_not_converged_retry_after_secs: u64,
}

impl Default for ServiceConfig {
    /// The published defaults, for a service built without a configured
    /// block — tests and pre-init contexts, reached through
    /// [`Service::new`].
    fn default() -> Self {
        Self {
            metadata_size_cap_bytes: DEFAULT_METADATA_SIZE_CAP_BYTES,
            covered_period_bounds: CoveredPeriodBounds::default(),
            max_batch_records: DEFAULT_MAX_BATCH_RECORDS,
            ingestion_quota: IngestionQuotaConfig::default(),
            unavailable_retry_after_secs: DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS,
            target_not_converged_retry_after_secs: DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS,
        }
    }
}

impl Service {
    /// Storage-plugin resolution is lazy — deferred to the first dispatch.
    ///
    /// Metrics default to a no-op adapter; production wires the real one
    /// via [`Service::new_with_metrics`]. The Type Resolver defaults to
    /// [`UnavailableDeclarationSource`] (a source that never succeeds, so
    /// its TTL/capacity are inert).
    ///
    /// Every configured knob comes from [`ServiceConfig::default`] — the
    /// published defaults, including the ingestion quota, so no test can
    /// accidentally exercise an unthrottled gear.
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
            reverse_resolver,
            ServiceConfig::default(),
        )
    }

    /// Construct the service with an explicit metrics sink, pre-built
    /// resolvers, and the projected `[usage_collector]` configuration.
    ///
    /// Used at gear bootstrap (`module.rs`) and by tests needing a real
    /// metrics adapter, a resolver over a fake `DeclarationSource`, or a
    /// non-default knob.
    ///
    /// Takes the finished `Arc<TypeResolver>` / `Arc<MeterReverseResolver>`
    /// / `Arc<dyn UsageCollectorMetrics>` rather than raw knobs, keeping
    /// this domain module free of any dependency on the concrete
    /// `types-registry` adapter.
    #[must_use]
    pub fn new_with_metrics(
        hub: Arc<ClientHub>,
        vendor: String,
        enforcer: PolicyEnforcer,
        metrics: Arc<dyn UsageCollectorMetrics>,
        type_resolver: Arc<TypeResolver>,
        reverse_resolver: Arc<MeterReverseResolver>,
        config: ServiceConfig,
    ) -> Self {
        let ServiceConfig {
            metadata_size_cap_bytes,
            covered_period_bounds,
            max_batch_records,
            ingestion_quota,
            unavailable_retry_after_secs,
            target_not_converged_retry_after_secs,
        } = config;
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

    /// Records the structural batch cap's rejection:
    /// `uc_ingestion_requests_total{outcome="rejected",
    /// error_category="validation"}` plus one structured log entry.
    ///
    /// Public rather than private to the cap arm because the REST edge
    /// (`handlers/usage_records.rs`) carries its own copy of the same cap
    /// ahead of decode, so both arms call this one recorder. Telemetry
    /// only — the authoritative check lives in the domain service. The
    /// per-record counter is untouched: no entry has been decoded yet, so
    /// `entry_type` does not apply.
    ///
    /// `op` distinguishes `PdpOp::Ingest` / `PdpOp::Backfill`, since
    /// `uc_ingestion_requests_total` carries no `origin` label.
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
    /// allowance as of `now`.
    ///
    /// Cost is the submitted entry count, not one per request — a
    /// per-request cost would let a caller buy `max_batch_records`x its
    /// budget by batching. Called after the entry cap and before the PDP
    /// call: with the startup rule `burst_entries >= max_batch_records`,
    /// that order keeps the rejection's retry delay finite and disposes of
    /// an empty submission before it can cost nothing.
    ///
    /// `now` is the request's single seeded instant, passed in rather than
    /// read here, so per-caller clock reads can't force the bucket's idle
    /// sweep (`crate::v2::domain::quota::sweep_if_due`) on every charge.
    ///
    /// Neither ingestion counter is touched here: the per-record one requires a
    /// validated `entry_type` this charge runs ahead of, and the request-level
    /// one belongs to the ingestion path's own quota arm, which alone knows the
    /// submission's outcome shape.
    // @cpt-dod:cpt-cf-usage-collector-dod-quota-subject-keyed:p2
    fn charge_ingestion_quota(
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
        let charged = self.quota.charge(subject, cost, now);

        // Sampled on every charge (admitted or refused): this gauge measures
        // residency, not rejections, so an admitted flood of distinct subjects
        // is exactly the map growth it watches.
        let active = self.quota.active_buckets();
        self.metrics
            .set_quota_buckets_active(u64::try_from(active).unwrap_or(u64::MAX));

        let Err(rejection) = charged else {
            return Ok(());
        };

        // By the submitted entry count, not by one: this counter carries
        // throttled volume.
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-rejected-volume-countable:p2
        self.metrics.record_quota_rejection(cost);
        // The subject and tenant cannot be metric labels, so the structured
        // log is where "which caller is being throttled" is answered.
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
    /// Batch ingestion entry.
    ///
    /// The live batch route: enforces the `1..=max_batch_records`
    /// structural cap, delegates to
    /// [`crate::v2::domain::ingestion::run_ingestion_pipeline`], and records
    /// the completion telemetry (`uc_ingestion_records_total` per entry,
    /// `uc_ingestion_requests_total`, `uc_ingestion_duration_seconds`, all
    /// `origin="live"`).
    ///
    /// # Errors
    ///
    /// * [`UsageCollectorError::InvalidArgument`] when the input violates
    ///   the structural cap.
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
        // Shared body lives in `create_usage_records_for_origin`.
        // `records.len()` is the submitted count here since there's no
        // wire decode in-process; a caller that decoded first must use
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
    /// submission itself and dropped entries that failed to decode, so
    /// `records` is shorter than what was actually received.
    ///
    /// `submitted_before_decode` is the entry count the submission arrived
    /// with, before any decode/validation/drop — not `records.len()`. Both
    /// the structural cap and the ingestion quota are charged against it, so a
    /// caller cannot escape the charge by submitting entries that fail later,
    /// and the check stays in the domain service rather than the handler so no
    /// in-process caller can miss it.
    ///
    /// `records` may be empty while `submitted_before_decode` is not — a
    /// submission whose every entry was refused at the wire edge. It is
    /// still charged (see `create_usage_records_for_origin`).
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
    /// submission itself, owing the same submitted-count obligation as
    /// [`Self::create_usage_records_with_submitted_count`]: backfill draws
    /// the same allowance as live emission, so a cheap escape on one route
    /// is a cheap escape on both.
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

    /// Bulk historical import of periods the live path rejects.
    ///
    /// Stamps `origin = backfill` and admits the covered periods the live
    /// past tolerance rejects; otherwise identical to
    /// [`Self::create_usage_records`]. May mix measurements and
    /// invalidations in one batch, both labelled `operation="backfill"` on
    /// the PDP instruments (see [`PdpOp::Backfill`]).
    ///
    // TODO(`cpt-cf-usage-collector-nfr-workload-isolation`): shares the
    // live path's runtime/connection-pool/fan-out budget with the live
    // route, nothing bounding it separately — a bulk import can still
    // degrade live ingestion p95. Needs a concurrent load test against
    // `cpt-cf-usage-collector-nfr-throughput-profile`.
    ///
    /// # Errors
    ///
    /// The same variants as [`Self::create_usage_records`].
    ///
    /// # Post-condition
    ///
    /// As [`Self::create_usage_records`].
    ///
    /// No hard rejection of an over-aged period exists anywhere in this gear,
    /// and the PDP action is chosen per entry from `window_end` via
    /// [`ingestion_action`](crate::v2::domain::covered_period::ingestion_action)
    /// rather than from the route — both known, tracked gaps.
    // @cpt-dod:cpt-cf-usage-collector-dod-backfill-route:p2
    // @cpt-algo:cpt-cf-usage-collector-algo-backfill-origin-marking:p2
    // @cpt-dod:cpt-cf-usage-collector-dod-backfill-origin-marker:p2
    pub async fn backfill_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        // `Backfill` comes from the route this wrapper *is*; everything
        // else is the live route's shared body. A gateway that decoded a
        // wire submission first calls
        // `backfill_usage_records_with_submitted_count` instead.
        let submitted_before_decode = records.len();
        self.create_usage_records_for_origin(
            ctx,
            records,
            RecordOrigin::Backfill,
            submitted_before_decode,
        )
        .await
    }

    /// The batch ingestion body, parameterized by the admitting path's
    /// `origin`: the structural `1..=max_batch_records` cap, the pipeline call,
    /// and the completion telemetry. Backfill is the same component under
    /// workload isolation, so `origin` is the whole difference and both entry
    /// points share this body.
    ///
    /// `submitted_before_decode` (what the caller sent) and `records.len()`
    /// (what survived decode) are not interchangeable: the cap and the
    /// quota read the first; the pipeline and per-record telemetry read
    /// the second. An all-malformed submission (`records` empty,
    /// `submitted_before_decode` not) is still charged and reports
    /// `outcome=partial`, with `uc_ingestion_records_total` silent — it is
    /// scoped to decoded entries. A partly-malformed submission completes as
    /// `partial` on the same evidence, the request counter not being decidable
    /// from the pipeline's verdicts alone.
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
        // The one request-level identifier for every completion point
        // below — a batch has no single resource/referenced type, but
        // always exactly one attributed tenant.
        let tenant = ctx.subject_tenant_id().to_string();
        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-request-reject
        // Judges what was SUBMITTED, not decoded — the same number the
        // REST edge caps on, so the two checks can't disagree.
        let actual = submitted_before_decode;
        if actual == 0 || actual > self.max_batch_records {
            // Request-wide rejection ("rejected whole with 400 on
            // `records`"); the per-record counter is untouched since
            // entry_type isn't validated yet.
            self.record_structural_cap_rejection(ctx, ingestion::pdp_op_for(origin));
            return Err(UsageCollectorError::invalid_batch_size(
                actual,
                1,
                self.max_batch_records,
            ));
        }
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-request-reject

        // Quota charge, after the cap and before the PDP call (DESIGN
        // §3.2). `now` is a single seeded instant shared with the
        // pipeline's covered-period admission, read after the cap so an
        // over-cap submission needs no clock read.
        let now = ingestion::acceptance_instant();
        // @cpt-flow:cpt-cf-usage-collector-flow-quota-throttled-submission:p2
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-charge-order:p2
        // @cpt-dod:cpt-cf-usage-collector-dod-quota-all-paths:p2
        if let Err(err) = self.charge_ingestion_quota(ctx, actual, now) {
            // This path's own counter to record — `quota` vs. the cap's
            // `validation` is what distinguishes the two whole-request
            // refusals.
            self.metrics.record_ingestion_request(
                IngestRequestOutcome::Rejected,
                IngestRequestErrorCategory::Quota,
            );
            log_operation_completed(
                OperationLogEntry::new(
                    ingestion::pdp_op_for(origin),
                    IngestRequestOutcome::Rejected,
                    IngestRequestErrorCategory::Quota,
                )
                .tenant(Some(tenant.as_str())),
            );
            // Returns before the batch-size observation below: a
            // whole-refused submission shouldn't count toward a histogram
            // of admitted batches.
            return Err(err);
        }

        // Shared by both batch-size observations below. Duration stays a
        // per-path `start.elapsed()` read since it genuinely differs.
        let submitted_as_u64 = u64::try_from(actual).unwrap_or(u64::MAX);

        // All entries failed to decode. Still charged — no exemption for
        // failing before the pipeline runs — and nothing is dispatched.
        //
        // **Three of four ingestion instruments record here — the fourth
        // stays silent on purpose, do not "fix" it:**
        // `uc_ingestion_records_total` is scoped to decoded entries, and this
        // submission has none. An operator reads the fold-rejection volume from
        // the shortfall of `uc_ingestion_batch_size` against it.
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
                    ingestion::pdp_op_for(origin),
                    IngestRequestOutcome::Partial,
                    IngestRequestErrorCategory::None,
                )
                .tenant(Some(tenant.as_str())),
            );
            return Ok(Vec::new());
        }

        // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-observe-batch-size
        // Samples the submitted count, not the post-decode one: one
        // observation per received batch submission, before per-record
        // processing. That is what makes the instrument carry fold-rejection
        // volume — this figure minus `uc_ingestion_records_total`'s total over
        // the same window is what a caller's fold refused, and the
        // all-malformed early-return branch samples the same value.
        self.metrics.observe_ingestion_batch_size(submitted_as_u64);
        // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-observe-batch-size

        // `entry_type` is captured per input index before `records` is moved
        // into the inner pipeline (the per-entry counter needs it after).
        let entry_types: Vec<EntryType> =
            records.iter().map(CreateUsageRecord::entry_type).collect();

        // Resolved before the pipeline since binding is request-level
        // work; `match` rather than `?` so the completion arm below still
        // records telemetry on a resolution failure.
        let result = match self.resolve_plugin_for(PluginOp::CreateUsageRecords).await {
            Ok(plugin) => {
                self.run_ingestion_pipeline(ctx, records, origin, now, plugin.as_ref())
                    .await
            }
            Err(e) => Err(e),
        };
        let seconds = start.elapsed().as_secs_f64();

        if let Ok(per_record) = &result {
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-records-counter
            for (record_result, entry_type) in per_record.iter().zip(entry_types.iter().copied()) {
                let (outcome, error_category) = match record_result {
                    Ok(_) => (RecordOutcome::Accepted, RecordErrorCategory::None),
                    Err(e) => (
                        RecordOutcome::Rejected,
                        observability::classify_record_error(e),
                    ),
                };
                self.metrics
                    .record_ingestion_record(outcome, entry_type, origin, error_category);
            }
            // @cpt-end:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-records-counter
            // @cpt-begin:cpt-cf-usage-collector-flow-emit-usage-record:p1:inst-emit-batch-request-completion-metrics
            // Two sources of rejection feed this counter: `per_record`'s
            // pipeline verdicts, and (via `submitted_before_decode`
            // exceeding the decoded count) the entries a wire caller's own
            // fold already refused — the only evidence of those this
            // method receives. For an in-process caller the two counts are
            // equal, so the second disjunct fires only for a
            // wire-decoding caller.
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
                    ingestion::pdp_op_for(origin),
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
                    ingestion::pdp_op_for(origin),
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

    /// Read a single `UsageRecord` by `uuid` from the bound storage plugin,
    /// scoped to the caller's compiled PDP grant.
    ///
    /// Authorizes first via [`authz::authorize_get_usage_record_scope`] (a
    /// pre-row PDP request, mirroring [`Self::list_usage_records`]'s
    /// posture). The projected scope is the whole filter passed to the
    /// SPI, so a row outside it answers `UsageRecordNotFound` exactly as a
    /// non-existent `id` would — this surface cannot be used as an
    /// existence oracle. A PDP deny collapses into that same `NotFound`
    /// (see [`collapse_deny_to_not_found`](error::collapse_deny_to_not_found)).
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
    // One plugin dispatch, no retry, no replication-watermark wait, result
    // passed through unchanged.
    // @cpt-flow:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-point-lookup-resolution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-point-lookup-exact-fact:p1
    // @cpt-algo:cpt-cf-usage-collector-algo-period-end-selection:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-period-end-selection:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-consistency-no-stronger-read-claim:p1
    pub async fn get_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UsageRecord, UsageCollectorError> {
        let start = std::time::Instant::now();

        // PDP step hoisted out of the body (still inside `start`'s span):
        // this function needs both the collapsed outcome the caller sees
        // and the uncollapsed one `uc_query_requests_total` labels from,
        // and `collapse_deny_to_not_found` discards the latter, so it's
        // read here first.
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
        let authorized = authorized.map_err(|e| error::collapse_deny_to_not_found(e, id));
        // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-get-record-pdp-deny
        // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-scope

        let result = async move {
            let (granted, scope_expr) = authorized?;

            let plugin = self.resolve_plugin_for(PluginOp::GetUsageRecord).await?;

            // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-resolve
            let stored = observability::instrument_spi(
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

            // `scope_expr` is the whole filter this row had to satisfy, and
            // applying it is the plugin's alone — nothing on this side makes
            // it. An implementation that ignores it turns the by-id read into
            // a cross-tenant read, so the owning tenant is re-decided here
            // against the grant it was projected from. Answered as the miss
            // the plugin owed, which is also what keeps this surface from
            // acting as an existence oracle.
            if !authz::scope_pins_tenant(&granted, stored.tenant_id) {
                tracing::warn!(
                    target: "authz",
                    entry_id = %id,
                    tenant_id = %stored.tenant_id,
                    "storage plugin served a usage_record outside the caller's granted \
                     tenant scope on the point lookup; refusing it gear-side"
                );
                return Err(UsageCollectorError::usage_record_not_found(id));
            }

            // @cpt-begin:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-project
            // No meter taken here, so the identifier is recovered from the
            // row's own reference via the reverse resolver
            // (`domain::meter_reverse`) — the one re-attachment site with
            // no expected reference to compare against first, since the
            // reference IS what derives the identifier.
            let gts_type_id = self
                .reverse_resolver
                .resolve(stored.gts_type_uuid)
                .await
                .map_err(|e| match e {
                    // Not the default 404: the row exists and the caller
                    // is entitled to it — this is gear-side corruption,
                    // not a caller-shaped miss.
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
            // fold, no marker, no suppression.
            // @cpt-algo:cpt-cf-usage-collector-algo-raw-ledger-projection:p1
            Ok(stored.into_usage_record(gts_type_id))
            // @cpt-end:cpt-cf-usage-collector-flow-lookup-entry-by-identifier:p1:inst-point-project
        }
        .await;

        // `point` labels `uc_query_requests_total` and
        // `uc_query_duration_seconds`; `observe_query_result_rows` is skipped
        // since a point lookup returns one record, not a page.
        //
        // The deny is labelled from `pdp_denied`, not `result`:
        // `collapse_deny_to_not_found` already rewrote a deny into the same
        // `NotFound` a genuine miss produces, and `pdp_denied` is the only
        // surviving witness of which one it was — which is why the collapse has
        // exactly one caller.
        let seconds = start.elapsed().as_secs_f64();
        let (outcome, error_category) = if pdp_denied {
            (RequestOutcome::Denied, QueryErrorCategory::Authz)
        } else {
            observability::classify_query_result(&result)
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

    /// Lazily resolves and returns the bound storage-plugin client.
    ///
    /// `GtsPluginSelector` resolves the instance id single-flight on first
    /// dispatch via [`Self::resolve_plugin`] and caches it for the
    /// service's lifetime; the scoped client is then looked up fresh via
    /// `ClientHub::try_get_scoped` on every call.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::TypesRegistryUnavailable`] when the registry
    /// call fails (the selector stays uncached so the next dispatch
    /// retries), [`DomainError::PluginNotFound`] when no instance matches
    /// the configured vendor, [`DomainError::InvalidPluginInstance`] on
    /// malformed instance content, and [`DomainError::PluginUnavailable`]
    /// when the scoped client is not registered under the resolved scope.
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
    pub async fn get_plugin(&self) -> Result<Arc<dyn UsageCollectorPluginV2>, DomainError> {
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
            .try_get_scoped::<dyn UsageCollectorPluginV2>(&scope);
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-scoped-lookup
        // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-binding-try-get-scoped

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
        // hold.
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

    /// Resolve the bound storage-plugin handle for an SPI dispatch,
    /// recording
    /// `uc_plugin_accept_errors_total{operation, error_category="unready"}`
    /// when the structural binding is unavailable. `op` labels the SPI
    /// method that would have been dispatched (the first, when a method
    /// issues several).
    ///
    /// Returns [`UsageCollectorError`] directly: the chokepoint every call
    /// site shares, so `unavailable_retry_after_secs` is applied here to
    /// [`Self::get_plugin`]'s `ServiceUnavailable`-shaped outcomes via
    /// [`lift_domain_error`]. Fail-closed with no substituted binding or
    /// retained stale client — `get_plugin` re-reads `try_get_scoped` on every
    /// call.
    // @cpt-algo:cpt-cf-usage-collector-algo-plugin-dispatch:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-plugin-fail-closed:p1
    // @cpt-begin:cpt-cf-usage-collector-algo-plugin-dispatch:p1:inst-dispatch-resolve
    pub(super) async fn resolve_plugin_for(
        &self,
        op: PluginOp,
    ) -> Result<Arc<dyn UsageCollectorPluginV2>, UsageCollectorError> {
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

    /// Resolves the bound storage-plugin instance id from `types-registry`,
    /// matching exactly on `UsageCollectorPluginSpecV2`'s schema identifier,
    /// then picking the lowest-priority instance via `choose_plugin_instance`.
    // @cpt-begin:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-first-dispatch
    // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-selector-query
    #[tracing::instrument(skip_all, fields(vendor = %self.vendor))]
    async fn resolve_plugin(&self) -> Result<String, DomainError> {
        info!("Resolving usage-collector storage plugin");

        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        let plugin_type_id = UsageCollectorPluginSpecV2::gts_type_id().clone();

        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{plugin_type_id}*")))
            .await
            .map_err(TypesRegistryError::from)?;

        // @cpt-begin:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-lowest-priority
        let gts_id = choose_plugin_instance::<UsageCollectorPluginSpecV2>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )?;
        // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-lowest-priority

        info!(plugin_gts_id = %gts_id, "Selected usage-collector storage plugin instance");

        Ok(gts_id)
    }
    // @cpt-end:cpt-cf-usage-collector-algo-plugin-binding-resolution:p1:inst-bind-selector-query
    // @cpt-end:cpt-cf-usage-collector-flow-plugin-vendor-selection:p1:inst-vendor-first-dispatch
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_tests.rs"]
pub(crate) mod service_tests;
