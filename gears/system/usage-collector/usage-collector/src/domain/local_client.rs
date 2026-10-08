//! Local (in-process) client for the usage-collector module.
//!
//! Registered in `ClientHub` during `init()` as the consumer-facing
//! `UsageCollectorClientV1`. The REST surface goes straight through the
//! domain [`Service`] and does not pass through this client.
//!
//! `list_usage_records` and `query_aggregated_usage_records` are both
//! realized by the `usage-query` feature and delegate to the
//! same-named methods on [`Service`] (PDP authorization, PDP constraint
//! composition into the `OData` filter, plugin SPI dispatch). The
//! mandatory `time_range` is forwarded verbatim: an in-process caller
//! already holds a validated [`TimeRange`], so there is nothing to parse
//! here and no second place a range could be dropped.
//!
//! `read_usage_feed` delegates the same way, straight to
//! [`Service::read_usage_feed`]: this client never touches
//! `api::rest::dto::FeedPageDto`, so an in-process caller receives the
//! domain [`FeedPage<CursorV1>`] `Service` builds, not a REST-wire
//! projection of it.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_odata::{CursorV1, ODataQuery, Page as ODataPage};
use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    AggregationDimension, AggregationResult, CreateUsageRecord, FeedPage, FeedStart,
    FeedSubscription, MetadataFilter, MeterTypeId, TimeRange, UsageCollectorClientV1,
    UsageCollectorError, UsageRecord,
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
    async fn create_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        self.svc.create_usage_records(ctx, records).await
    }

    // The backfill route is a DIFFERENT service method, not
    // `create_usage_records` under a flag: the origin marker each entry
    // carries is decided by which one is called here, and forwarding to the
    // live batch would stamp `live` on an import and refuse every
    // correction of closed history.
    async fn backfill_usage_records(
        &self,
        ctx: &SecurityContext,
        records: Vec<CreateUsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorError>>, UsageCollectorError> {
        self.svc.backfill_usage_records(ctx, records).await
    }

    async fn get_usage_record(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UsageRecord, UsageCollectorError> {
        self.svc.get_usage_record(ctx, id).await
    }

    // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-submit
    async fn query_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError> {
        self.svc
            .query_aggregated_usage_records(
                ctx,
                gts_type_id,
                time_range,
                query,
                metadata_filter,
                group_by,
            )
            .await
    }
    // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-agg-submit

    // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-submit
    async fn list_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorError> {
        self.svc
            .list_usage_records(ctx, gts_type_id, time_range, query, metadata_filter)
            .await
    }
    // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-submit

    // @cpt-flow:cpt-cf-usage-collector-component-feed-gateway:p1
    // @cpt-flow:cpt-cf-usage-collector-seq-read-feed:p1
    async fn read_usage_feed(
        &self,
        ctx: &SecurityContext,
        subscription: &FeedSubscription,
        start: FeedStart<&CursorV1>,
        until: Option<&CursorV1>,
        limit: Option<u64>,
    ) -> Result<FeedPage<CursorV1>, UsageCollectorError> {
        self.svc
            .read_usage_feed(ctx, subscription, start, until, limit)
            .await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "local_client_tests.rs"]
mod local_client_tests;
