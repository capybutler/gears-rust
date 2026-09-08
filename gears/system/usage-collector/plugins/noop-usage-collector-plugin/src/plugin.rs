//! No-op storage backend for the Usage Collector storage Plugin SPI.
//!
//! [`NoopBackend`] implements [`usage_collector_sdk::UsageCollectorPluginV1`]
//! and persists nothing. It exists so the plugin-host binding resolves
//! end-to-end in development and testing without a real database backend:
//! the plugin still performs the full GTS registration handshake and
//! registers its scoped client in `ClientHub`, but every SPI operation
//! returns a well-formed default response. MUST NOT be used in production.

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, MetadataFilter, MeterTypeId,
    TimeRange, UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord,
};

#[derive(Debug, Default)]
pub struct NoopBackend;

impl NoopBackend {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for NoopBackend {
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        Ok(record)
    }

    async fn create_usage_records(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        if records.is_empty() {
            return Err(UsageCollectorPluginError::internal(
                "create_usage_records called with an empty batch (host-contract breach)",
            ));
        }
        Ok(records.into_iter().map(Ok).collect())
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        _scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::UsageRecordNotFound { id })
    }

    /// Persists nothing, so every fold is taken over an empty selection and
    /// the withdrawal exclusion the SPI states has nothing to leave out.
    ///
    /// The empty `buckets` vector is this backend's well-formed default,
    /// **not** the shape a conforming plugin answers with: the no-grouping
    /// case is a single bucket carrying an empty `key`
    /// ([`AggregationResult`]), whose value is absent for every fold but
    /// `COUNT`. The gear does read the count — it refuses a result over the
    /// declared aggregate-bucket cap and observes the count as result-row
    /// telemetry — and it passes the buckets through to the wire, so a
    /// caller sees the difference too. Zero is under every cap and reads as
    /// an empty result, which is why the default is harmless here and only
    /// here: this is a backend that MUST NOT be used in production.
    async fn query_aggregated_usage_records(
        &self,
        _gts_type_id: MeterTypeId,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        Ok(AggregationResult {
            buckets: Vec::new(),
        })
    }

    async fn list_usage_records(
        &self,
        _gts_type_id: MeterTypeId,
        _time_range: TimeRange,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        Ok(ODataPage::empty(0))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "plugin_tests.rs"]
mod plugin_tests;
