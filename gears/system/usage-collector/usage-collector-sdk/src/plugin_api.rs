//! Storage Plugin SPI for the Usage Collector.

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use crate::error::UsageCollectorPluginError;
use crate::models::{
    AggregationDimension, AggregationFold, AggregationResult, MetadataFilter, MeterTypeId,
    UsageRecord,
};

/// Backend storage adapter trait implemented by
/// `usage-collector-plugin-<backend>` crates.
///
/// Plugins are pure persistence: authorization and shape validation are
/// the gateway's responsibility.
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-contract-storage-plugin:p1
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-nfr-plugin-contract-stability:p1
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-principle-contract-stability:p1
// @cpt-dod:cpt-cf-usage-collector-dod-foundation-adr-contract-stability:p1
#[async_trait]
pub trait UsageCollectorPluginV1: Send + Sync + 'static {
    /// Persist a single usage record.
    ///
    /// An exact-equality retry under the same idempotency key returns
    /// the previously persisted row.
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError>;

    /// Persist a batch of usage records.
    ///
    /// Per-record outcomes are aligned with the input order.
    async fn create_usage_records(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>;

    /// Get a single usage record by its `id`.
    ///
    /// `scope` is the caller's compiled PDP scope, projected into a
    /// `toolkit_odata` filter expression by
    /// `authz::scope_to_odata_filter` (gateway-side; never a plugin
    /// concern). The point lookup carries no caller-supplied filter of its
    /// own, so `scope` is the *whole* filter the row must satisfy — a row
    /// whose attribution tuple falls outside it MUST NOT be returned; the
    /// plugin reports `UsageRecordNotFound` exactly as it would for an
    /// `id` that does not exist at all. This is what keeps the by-id
    /// surface from acting as an existence oracle: the caller cannot tell
    /// "exists but not yours" apart from "does not exist".
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError>;

    /// Compute the given fold over the authorized scope.
    ///
    /// The fold arrives as a parameter: declarations never reach the SPI,
    /// so the plugin stays pure persistence and never resolves a type
    /// itself. The time window is expressed inside `query.filter` as a
    /// `created_at ge … and created_at lt …` predicate; there is no
    /// separate typed parameter.
    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError>;

    /// Keyset-paginated list of usage records.
    ///
    /// `query.order` is guaranteed non-empty (the gateway defaults to
    /// `(created_at asc, id asc)` if the caller omits `$orderby`), so
    /// plugins MUST honour it for stable pagination.
    async fn list_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;

    /// Deactivate a usage record.
    ///
    /// On `Ok(())`, the targeted record and every active record that
    /// compensates it are atomically flipped to `inactive`.
    async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError>;
}
