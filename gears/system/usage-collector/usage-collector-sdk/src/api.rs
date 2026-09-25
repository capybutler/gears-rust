//! Consumer-facing SDK trait for the Usage Collector.

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::error::UsageCollectorError;
use crate::models::{
    AggregationDimension, AggregationResult, CreateUsageRecord, MetadataFilter, MeterTypeId,
    UsageRecord,
};
use crate::time_range::TimeRange;

/// Consumer-facing API for Usage Collector operations.
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-entity-security-context:p1
#[async_trait]
pub trait UsageCollectorClientV1: Send + Sync + 'static {
    /// Create a single usage record.
    ///
    /// Takes the identity-free [`CreateUsageRecord`]: the returned record's
    /// `id` is derived deterministically from the 5-tuple dedup identity
    /// (tenant, meter type, idempotency key and both covered-period bounds),
    /// never supplied by the caller. An exact-equality retry under the same
    /// dedup identity returns the previously persisted record; a
    /// canonical-field mismatch surfaces as
    /// [`UsageCollectorError::Conflict`].
    ///
    /// A covered period that is inverted, or that carries a bound finer
    /// than microsecond precision, surfaces as
    /// [`UsageCollectorError::InvalidArgument`] — both are rejected before
    /// the identity derivation runs, never truncated
    /// (`cpt-cf-usage-collector-adr-record-identity-derivation`).
    async fn create_usage_record(
        &self,
        ctx: &SecurityContext,
        record: CreateUsageRecord,
    ) -> Result<UsageRecord, UsageCollectorError>;

    /// Create a batch of usage records.
    ///
    /// Takes identity-free [`CreateUsageRecord`] submissions (see
    /// [`Self::create_usage_record`]). Per-record outcomes are aligned with
    /// the input order.
    async fn create_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError>;

    /// Bulk historical import on its own route
    /// (`cpt-cf-usage-collector-adr-backfill-isolation`).
    ///
    /// Stamps [`RecordOrigin::Backfill`] and admits the covered periods the
    /// live past tolerance rejects. Validation is otherwise identical to
    /// [`Self::create_usage_records`], the future tolerance included: this
    /// route lifts the past bound and nothing else. Batch-only — there is
    /// no single-entry counterpart, because an import is a batch.
    ///
    /// It is on this trait rather than on REST alone because it is the only
    /// route reaching past the live past tolerance, and a defect is
    /// normally found days later rather than hours later. An in-process
    /// emitter needs it to import history **and to withdraw it** — an
    /// invalidation whose covered period is older than the live past
    /// tolerance is refused there and belongs here, which makes correcting
    /// closed history the ordinary use of this route rather than an exotic
    /// one. Confining it to REST would turn every such correction into an
    /// operator escalation.
    ///
    /// An entry whose covered period ends further back than the
    /// deployment's configured backfill window is authorized against the
    /// `usage_record` `backfill` action instead of `create` — the elevated
    /// grant an import job holds and a plain emitter does not. One batch
    /// may mix the two.
    ///
    /// The ADR's *workload* isolation is a gear-level obligation and is
    /// **not implemented**: this route shares the live path's runtime,
    /// connection pool and fan-out budget, so a bulk import can still
    /// degrade live ingestion latency. What it does own is its origin
    /// marker, its covered-period bounds and its own PDP labelling.
    ///
    /// Per-record outcomes are aligned with the input order, as on
    /// [`Self::create_usage_records`].
    ///
    /// [`RecordOrigin::Backfill`]: crate::RecordOrigin::Backfill
    async fn backfill_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError>;

    /// Get a single usage record by its `id`.
    ///
    /// Returns the persisted record on `Ok`; an unknown `id` surfaces
    /// as [`UsageCollectorError::NotFound`]. The read runs under the
    /// caller's compiled PDP scope, so a record outside it is
    /// [`UsageCollectorError::NotFound`] too — this surface is never an
    /// existence oracle.
    async fn get_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UsageRecord, UsageCollectorError>;

    /// Aggregated query over one meter and one range.
    ///
    /// Carries no aggregation parameter: the fold is resolved from the
    /// queried type's declaration, so no request is well-formed and
    /// semantically wrong. A withdrawn record and its invalidation each
    /// contribute nothing.
    ///
    /// `time_range` is mandatory and typed — it is never a `$filter`
    /// conjunct (DESIGN §3.3 rule 5), and a predicate naming either
    /// covered-period bound is rejected rather than honoured. An entry is
    /// selected when the end of its covered period falls in the range,
    /// `from <= window_end < to`, whatever the length of that period
    /// (`cpt-cf-usage-collector-adr-window-end-selection`).
    async fn query_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError>;

    /// Keyset-paginated ledger read over one meter and one range.
    ///
    /// `time_range` is mandatory and typed, selecting on the covered-period
    /// end exactly as on [`Self::query_aggregated_usage_records`]. Entries
    /// are returned as persisted — a withdrawn record and its invalidation
    /// both appear, because this is a ledger path rather than a derived
    /// view; excluding them from a locally computed fold is the reader's
    /// obligation.
    async fn list_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError>;
}
