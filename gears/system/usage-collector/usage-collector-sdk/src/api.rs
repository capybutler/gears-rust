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

/// Consumer-facing API for Usage Collector operations.
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-entity-security-context:p1
#[async_trait]
pub trait UsageCollectorClientV1: Send + Sync + 'static {
    /// Create a single usage record.
    ///
    /// Takes the identity-free [`CreateUsageRecord`]: the returned record's
    /// `id` is derived deterministically from the dedup key, never supplied by
    /// the caller. An exact-equality retry under the same idempotency key
    /// returns the previously persisted record; a canonical-field mismatch
    /// surfaces as [`UsageCollectorError::Conflict`].
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
    /// as [`UsageCollectorError::NotFound`].
    async fn get_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UsageRecord, UsageCollectorError>;

    /// Aggregated query over one meter.
    ///
    /// Carries no aggregation parameter: the fold is resolved from the
    /// queried type's declaration, so no request is well-formed and
    /// semantically wrong. A withdrawn record and its invalidation each
    /// contribute nothing.
    async fn query_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError>;

    /// Keyset-paginated list of usage records.
    async fn list_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError>;

    /// Deactivate a usage event.
    ///
    /// On success the targeted record and every active referencing
    /// compensation row are flipped to `inactive` atomically.
    // @cpt-begin:cpt-cf-usage-collector-state-event-deactivation-record-lifecycle:p1:inst-state-no-reactivation
    async fn deactivate_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), UsageCollectorError>;
    // @cpt-end:cpt-cf-usage-collector-state-event-deactivation-record-lifecycle:p1:inst-state-no-reactivation
}
