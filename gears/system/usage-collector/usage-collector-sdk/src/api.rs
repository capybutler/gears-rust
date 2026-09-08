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
