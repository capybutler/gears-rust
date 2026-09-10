use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationResult, AggregationSpec, MetadataFilter, MeterTypeId, TimeRange,
    UsageCollectorPluginError, UsageRecord, UsageTypeGtsId,
};

/// Persistence + query operations on `usage_records`. Implemented by infra.
#[async_trait]
pub trait RecordStore: Send + Sync + 'static {
    async fn create(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError>;
    async fn create_batch(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>;
    /// Read one entry by its `id`, intersected with the caller's compiled
    /// PDP scope.
    ///
    /// `scope` is the *whole* filter the row must satisfy: this path carries
    /// no caller-supplied `$filter` of its own. An entry outside it reads as
    /// [`UsageCollectorPluginError::UsageRecordNotFound`], indistinguishable
    /// from one that was never stored.
    async fn get(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError>;
    /// Keyset-paginated ledger read over one meter and one covered-period
    /// range.
    ///
    /// `gts_type_id` and `time_range` are typed parameters rather than
    /// `$filter` conjuncts, and the gateway refuses a predicate naming either
    /// covered-period bound. An entry is selected when the end of its period
    /// falls in the range, `from <= window_end < to`, whatever the length of
    /// that period (`cpt-cf-usage-collector-adr-window-end-selection`).
    ///
    /// Entries are returned as persisted: a withdrawn record and the
    /// invalidation that withdrew it both appear, because this is a ledger path
    /// rather than a derived view.
    async fn list(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;
    async fn aggregate(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        spec: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError>;
}
