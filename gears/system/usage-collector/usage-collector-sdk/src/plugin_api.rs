//! Storage Plugin SPI for the Usage Collector.

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use crate::error::UsageCollectorPluginError;
use crate::models::{
    AggregationDimension, AggregationFold, AggregationResult, MetadataFilter, MeterTypeId,
    UsageRecord,
};
use crate::time_range::TimeRange;

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

    /// Compute the given fold over the authorized scope and `time_range`.
    ///
    /// The fold arrives as a parameter: declarations never reach the SPI,
    /// so the plugin stays pure persistence and never resolves a type
    /// itself.
    ///
    /// `time_range` is a typed parameter and never appears in
    /// `query.filter`. The plugin MUST select an entry when
    /// `from <= window_end < to`
    /// (`cpt-cf-usage-collector-adr-window-end-selection`): the predicate
    /// reads the period end alone, needs no case for a point event
    /// (`window_start == window_end`), and MUST NOT match by overlap or by
    /// containment — those make adjacent ranges double count or drop
    /// entries. No selection predicate reads `window_start`.
    /// [`TimeRange::contains_window_end`] is the reference spelling for an
    /// in-process implementation; a SQL-backed plugin restates the same
    /// boundary in its own `WHERE` clause.
    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError>;

    /// Keyset-paginated ledger read over the authorized scope and
    /// `time_range`.
    ///
    /// `time_range` selects on the covered-period end under the same
    /// obligation as [`Self::query_aggregated_usage_records`], and is
    /// likewise absent from `query.filter`.
    ///
    /// A non-empty `query.order` MUST be honoured: it is the keyset the
    /// page continuation is built from, so ignoring it drops rows across a
    /// page boundary.
    ///
    /// `query.order` is still normalized on the REST path only, where the
    /// gateway appends the canonical unique `(created_at, id)` suffix in
    /// the caller's sort direction; an in-process caller reaches the
    /// service directly and may pass an [`ODataQuery`] carrying no order at
    /// all, so a plugin cannot yet rely on the slot being populated and
    /// falls back to its own deterministic keyset when it is empty. A
    /// later commit in this slice moves that normalization behind the
    /// service, making the guarantee unconditional for every caller — as
    /// DESIGN §3.3 states it, without a per-path caveat.
    async fn list_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;

    /// Deactivate a usage record.
    ///
    /// On `Ok(())`, the targeted record and every active record that
    /// compensates it are atomically flipped to `inactive`.
    async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError>;
}
