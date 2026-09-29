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

    /// Not built yet: nothing reads the feed order or the retention marks, so
    /// this backend can issue no position and decide no retention refusal.
    ///
    /// **What is missing is the read, not the schema.**
    /// `migrations/0001_init.sql` declares `xact_id`, the inserting
    /// transaction's id, indexes it as
    /// `usage_records_feed_idx (gts_type_id, xact_id, id)`, and declares the
    /// `usage_feed_retention_marks` table. That order is comparable across a
    /// whole subscription and its encoded size does not grow with that
    /// subscription's breadth, which is the bound the gear's DESIGN places on a
    /// position and the reason a position keyed per tenant cannot serve. What is
    /// left is the page protocol that reads the order, and the sweep that raises
    /// a mark for this method to refuse against: slice 3.
    ///
    /// `Internal` rather than `Transient` because there is nothing to retry:
    /// the method will answer once the page protocol and the sweep's
    /// mark-raising exist, and a `Transient` here would put a caller into a
    /// retry loop against a permanent condition.
    async fn read_feed_page(
        &self,
        _subscription: &[MeterTypeId],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "the TimescaleDB backend does not serve the usage feed yet: its schema declares the \
             subscription-wide (xact_id, id) order and the retention-mark table, but no page \
             protocol reads them and no sweep raises a mark",
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
    ///   `usage_records_tenant_type_window_idx`, which is
    ///   `(tenant_id, gts_type_id, window_end DESC)` exactly — the range count
    ///   is a scan of one index interval and the watermark is its leading edge.
    ///   No tie-break column trails the period end any more, which costs this
    ///   read nothing: neither figure needs one.
    /// * `MAX(accepted_at)` is served by
    ///   `usage_records_watermark_idx (gts_type_id, tenant_id, accepted_at
    ///   DESC)`, which `0001_init.sql` declares for exactly this read. It
    ///   needed an index of its own because no other index, and no
    ///   constraint-backed one, names `accepted_at` at all: the watermark is
    ///   unbounded by the range, so serving it without a scan means reading
    ///   the leading edge of an index ordered by `accepted_at` within one
    ///   `(gts_type_id, tenant_id)` group, and nothing else here is.
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
             watermarks. Every index they would need is declared: the count and \
             max(window_end) by the (tenant_id, gts_type_id, window_end) index, and \
             max(accepted_at) by the (gts_type_id, tenant_id, accepted_at) one. The query that \
             reads them is what is missing",
        ))
    }
}
