//! Local (in-process) client for the usage-collector module.
//!
//! Registered in `ClientHub` during `init()` as the consumer-facing
//! `UsageCollectorClientV1`. The REST surface goes straight through the
//! domain [`Service`] and does not pass through this client.
//!
//! `list_usage_records` and `query_aggregated_usage_records` are both
//! realized by the `usage-query` feature and delegate to the
//! same-named methods on [`Service`] (PDP authorization, PDP constraint
//! composition into the `OData` filter, plugin SPI dispatch).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    AggregationDimension, AggregationResult, CreateUsageRecord, MetadataFilter, MeterTypeId,
    UsageCollectorClientV1, UsageCollectorError, UsageRecord,
};
use uuid::Uuid;

use super::Service;

/// Local client wrapping the usage-collector service.
#[domain_model]
pub struct UsageCollectorLocalClient {
    svc: Arc<Service>,
}

impl UsageCollectorLocalClient {
    #[must_use]
    pub fn new(svc: Arc<Service>) -> Self {
        Self { svc }
    }
}

#[async_trait]
impl UsageCollectorClientV1 for UsageCollectorLocalClient {
    async fn create_usage_record(
        &self,
        ctx: &SecurityContext,
        record: CreateUsageRecord,
    ) -> Result<UsageRecord, UsageCollectorError> {
        self.svc.create_usage_record(ctx, record).await
    }

    async fn create_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        self.svc.create_usage_records(ctx, records).await
    }

    async fn get_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UsageRecord, UsageCollectorError> {
        self.svc.get_usage_record(ctx, id).await
    }

    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-request-received
    async fn query_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError> {
        self.svc
            .query_aggregated_usage_records(ctx, gts_type_id, query, metadata_filter, group_by)
            .await
    }
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-aggregated:p1:inst-aggregated-request-received

    // @cpt-begin:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-request-received
    async fn list_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError> {
        self.svc
            .list_usage_records(ctx, gts_type_id, query, metadata_filter)
            .await
    }
    // @cpt-end:cpt-cf-usage-collector-flow-usage-query-query-raw:p1:inst-raw-request-received

    async fn deactivate_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), UsageCollectorError> {
        self.svc.deactivate_usage_record(ctx, id).await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "local_client_tests.rs"]
mod local_client_tests;
