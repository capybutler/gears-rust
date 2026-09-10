use std::sync::Arc;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationResult, AggregationSpec, MetadataFilter, MeterTypeId, TimeRange,
    UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord, UsageTypeGtsId,
};

use crate::domain::ports::RecordStore;

/// The single implementation of `UsageCollectorPluginV1`. Delegates every SPI
/// method to the [`RecordStore`] port.
#[domain_model]
pub(crate) struct StorageAdapter {
    record: Arc<dyn RecordStore>,
}

impl StorageAdapter {
    #[must_use]
    pub(crate) fn new(record: Arc<dyn RecordStore>) -> Self {
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
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.record.get(id, scope).await
    }

    async fn query_aggregated_usage_records(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        aggregation: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.record
            .aggregate(gts_id, query, metadata_filter, aggregation)
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
}
