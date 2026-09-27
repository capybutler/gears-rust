use std::sync::Arc;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, FeedPage, FeedPosition, FeedStart,
    MetadataFilter, MeterTypeId, ReconciliationMetadata, TimeRange, UsageCollectorPluginError,
    UsageCollectorPluginV1, UsageRecord,
};

use crate::domain::ports::RecordStore;

/// The single implementation of `UsageCollectorPluginV1`. Delegates every SPI
/// method to the [`RecordStore`] port.
///
/// `pub` for the same reason [`crate::domain`] is: this is the only type in
/// the crate that implements the SPI, so the DESIGN section 3.3 contract
/// suite — which takes a `&dyn UsageCollectorPluginV1` — cannot be run from
/// `tests/contract_conformance_pg.rs` unless the test crate can name it. Not
/// public API; `#[doc(hidden)]` on the module keeps it off the rendered
/// surface, and external consumers reach the SPI through `ClientHub`.
#[domain_model]
pub struct StorageAdapter {
    record: Arc<dyn RecordStore>,
}

impl StorageAdapter {
    #[must_use]
    pub fn new(record: Arc<dyn RecordStore>) -> Self {
        Self { record }
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for StorageAdapter {
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.record.create(record).await
    }

    async fn create_usage_records(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        self.record.create_batch(records).await
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        // Single primary, one pool: every read is converged, so the flag changes nothing (DESIGN §3.3 converged-only lookups).
        self.record.get(id, scope).await
    }

    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.record
            .aggregate(
                gts_type_id,
                time_range,
                fold,
                query,
                metadata_filter,
                group_by,
            )
            .await
    }

    async fn list_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        self.record
            .list(gts_type_id, time_range, query, metadata_filter)
            .await
    }

    /// Not built yet: this backend has no subscription-wide feed order, no
    /// retention-mark table and no page protocol, so it can issue no position
    /// and decide no retention refusal.
    ///
    /// **The ordering column is not what is missing.**
    /// `migrations/0001_init.sql` already declares `acceptance_sequence` and
    /// indexes it for exactly this read. What it does not give is a
    /// *subscription-wide* order: `usage_acceptance_sequence` keys its counter
    /// on `(tenant_id, gts_type_id)`, so the sequence is monotonic per scope
    /// only. A `FeedPosition` must be comparable across a whole subscription
    /// without its encoded size growing with that subscription's breadth, and
    /// the gear DESIGN says outright that a position keyed per tenant fails
    /// that bound. The cross-subscription key is the thing to design here; the
    /// column is already there.
    ///
    /// `Internal` rather than `Transient` because there is nothing to retry:
    /// the method will answer once those three exist, and a `Transient` here
    /// would put a caller into a retry loop against a permanent condition.
    async fn read_feed_page(
        &self,
        _subscription: &[MeterTypeId],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "the TimescaleDB backend does not serve the usage feed yet: its acceptance_sequence \
             is monotonic per (tenant, meter) rather than across a subscription, and it has no \
             retention-mark table and no page protocol",
        ))
    }

    /// Not built yet: this backend reports no reconciliation metadata.
    ///
    /// Three figures are wanted, over the `(tenant_id, gts_type_id)` scope:
    /// the accepted count the range selects, `MAX(accepted_at)` and
    /// `MAX(window_end)`, the last two unbounded by the range. Unlike the feed
    /// this needs no new ordering — one `SELECT COUNT(*) FILTER (...),
    /// MAX(accepted_at), MAX(window_end)` expresses all three.
    ///
    /// What the migrations do and do not already serve, read off
    /// `0001_init.sql` rather than assumed:
    ///
    /// * The count and `MAX(window_end)` are served by
    ///   `usage_records_tenant_type_window_idx`, whose leading columns are
    ///   `(tenant_id, gts_type_id, window_end DESC)` — the range count is a
    ///   scan of one index interval and the watermark is its leading edge.
    /// * `MAX(accepted_at)` is served by **no** index: `accepted_at` appears
    ///   in the schema as a column only, and none of the four indexes names
    ///   it. `usage_records_acceptance_seq_idx` is not a substitute —
    ///   `acceptance_sequence` is plugin-assigned at write while `accepted_at`
    ///   is stamped upstream by the Ingestion Gateway and written as given, so
    ///   the two orders are not guaranteed to agree.
    ///
    /// `Internal` rather than `Transient` for the same reason
    /// [`Self::read_feed_page`] is — retrying cannot make an unbuilt method
    /// answer.
    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _gts_type_id: MeterTypeId,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "the TimescaleDB backend does not report reconciliation metadata yet: it computes \
             neither the per-scope accepted count nor the max(accepted_at) and max(window_end) \
             watermarks. The count and max(window_end) are already served by the \
             (tenant_id, gts_type_id, window_end) index; max(accepted_at) has no index",
        ))
    }
}
