use std::sync::Arc;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, FeedPage, FeedPosition, FeedStart,
    Keyset, MetadataFilter, MeterRef, ReconciliationMetadata, RecordPage, StoredUsageRecord,
    TimeRange, UsageCollectorPluginError, UsageCollectorPluginV1,
};

use crate::domain::feed_position::{decode_position, encode_position};
use crate::domain::ports::{MAX_PAGE_SIZE, RecordStore};

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
// @cpt-dod:cpt-cf-uc-plugin-dod-spi-conformance-release-gate:p1
impl UsageCollectorPluginV1 for StorageAdapter {
    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        self.record.create_batch(records).await
    }

    // @cpt-flow:cpt-cf-uc-plugin-flow-resolve-target-by-lookup:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-converged-only-semantics:p1
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        // Single primary, one pool: every read is converged, so the flag changes nothing (DESIGN §3.3 converged-only lookups).
        self.record.get(id, scope).await
    }

    async fn query_aggregated_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.record
            .aggregate(
                meter.uuid,
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
        meter: &MeterRef,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        self.record
            .list(meter.uuid, time_range, query, metadata_filter, keyset)
            .await
    }

    /// Reads one feed page, decoding the plugin's own positions on the way in
    /// and encoding them on the way out.
    ///
    /// `FeedStart` is `#[non_exhaustive]` per the SDK's §2.2 additive-evolution
    /// constraint, so a start mode a later gear version adds reaches the
    /// wildcard arm and fails loudly as `Internal` rather than being silently
    /// read as one of the two this version knows.
    async fn read_feed_page(
        &self,
        subscription: &[MeterRef],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        // Refused rather than clamped, and the asymmetry with the ledger page
        // is the SPI's own: `effective_page_size` clamps a caller's `$top`
        // into `[1, MAX_PAGE_SIZE]`, while `read_feed_page`'s contract puts "a
        // `limit` outside the published bound" under `Internal`, beside a start
        // mode this plugin does not know. A clamp here would hand a consumer a
        // page size it never asked for on a path whose whole purpose is an
        // exactly-resumable scan, and a `limit` of 0 would return nothing for
        // ever while looking healthy.
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(UsageCollectorPluginError::internal(format!(
                "a feed page limit of {limit} is outside the published bound of \
                 1..={MAX_PAGE_SIZE}"
            )));
        }

        let after = match start {
            FeedStart::Oldest => None,
            FeedStart::After(position) => {
                Some(decode_position(&position).map_err(UsageCollectorPluginError::internal)?)
            }
            // A start mode this version does not know. `Internal`, not a guess.
            _ => {
                return Err(UsageCollectorPluginError::internal(
                    "this backend serves only the `Oldest` and `After` feed start modes; a \
                     start mode it does not know is a host-contract breach rather than a \
                     condition to retry",
                ));
            }
        };
        let until = until
            .as_ref()
            .map(decode_position)
            .transpose()
            .map_err(UsageCollectorPluginError::internal)?;

        let gts_type_uuids: Vec<Uuid> = subscription.iter().map(|meter| meter.uuid).collect();
        let page = self
            .record
            .feed_page(&gts_type_uuids, scope, after, until, limit)
            .await?;

        Ok(FeedPage {
            entries: page.entries,
            next: page.next.map(|(xact_id, id)| encode_position(xact_id, id)),
        })
    }

    /// Per-scope ingestion counters and watermarks — the fourth read path,
    /// and the one the other three share no statement with (spec §9.1).
    ///
    /// Four figures over the `(tenant_id, gts_type_uuid)` scope: the accepted
    /// count the range selects, a fold-appropriate summary over the same
    /// range, and two watermarks unbounded by it. The summary needs **no new
    /// index**: the `LATEST` tie-break keys (`window_end`, `accepted_at`,
    /// `id`) are read off the rows a scope-and-range predicate has already
    /// selected rather than sought through an index of their own, which is
    /// what `0001_init.sql:247-255`'s comment on
    /// `usage_records_tenant_type_window_idx` already argues for the
    /// aggregate path this summary shares its statement shape with. The two
    /// watermarks read their own indexes by name:
    /// `usage_records_watermark_idx (gts_type_uuid, tenant_id, accepted_at
    /// DESC)` for `max(accepted_at)`, and
    /// `usage_records_tenant_type_window_idx (tenant_id, gts_type_uuid,
    /// window_end DESC)` for `max(window_end)`.
    ///
    /// Delegates to [`RecordStore::reconciliation`], which runs two
    /// statements — spec §9.3's S1∧S2 (the ranged scan producing
    /// `accepted_count` and the summary together) and S3 (the two watermarks,
    /// unbounded, kept off the ranged scan so each stays index-reachable) —
    /// rather than the single query a naive read of the DESIGN §3.6 sequence
    /// diagram's one `Rec->>DB` arrow might suggest: that arrow is the
    /// diagram's usual level of abstraction for "the store reads what it
    /// needs", not a one-round-trip mandate, and the prose beside it already
    /// separates "count(*) and fold summary ... `max(accepted_at)`,
    /// `max(window_end)` unbounded" into the same two groups these two
    /// statements are.
    ///
    /// **These four figures are not evidence that the feed delivered
    /// everything**, and neither this method nor its caller may present them
    /// as such: they describe what the ledger holds for one scope, and feed
    /// completeness is a property of the feed's own page protocol.
    // @cpt-dod:cpt-cf-uc-plugin-dod-no-completeness-claim:p2
    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        self.record
            .reconciliation(tenant_id, meter, time_range, fold, scope)
            .await
    }
}
