use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, MetadataFilter, MeterTypeId,
    TimeRange, UsageCollectorPluginError, UsageRecord,
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
    /// rather than a derived view — though **not necessarily on one page**. The
    /// pair shares a `window_end` but not an `id`, and an admissible order
    /// names both, so a page boundary can fall between them whichever of the
    /// two the order leads with. A consumer folding the pair out folds over a
    /// range it has read whole, not over a single page.
    async fn list(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;
    /// Fold one meter's entries over one covered-period range, optionally
    /// grouped.
    ///
    /// `fold` and `group_by` are typed parameters: a declaration never reaches
    /// this port, so the store resolves no usage type and stays pure
    /// persistence. `time_range` selects on the covered-period end under the
    /// same `from <= window_end < to` obligation [`RecordStore::list`] carries.
    ///
    /// A withdrawn pair contributes nothing — both the invalidation entry and
    /// the record it names — which is the one rule this path applies that the
    /// ledger paths do not
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    ///
    /// An empty `group_by` yields a **single** bucket carrying an empty key,
    /// never an empty bucket list.
    async fn aggregate(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError>;
}
