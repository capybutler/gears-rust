//! No-op storage backend for the Usage Collector storage Plugin SPI.
//!
//! [`NoopBackend`] implements [`usage_collector_sdk::UsageCollectorPluginV1`]
//! and persists nothing. It exists so the plugin-host binding resolves
//! end-to-end in development and testing without a real database backend:
//! the plugin still performs the full GTS registration handshake and
//! registers its scoped client in `ClientHub`, but every SPI operation
//! returns a well-formed default response. MUST NOT be used in production.

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, FeedPage, FeedPosition, FeedStart,
    MetadataFilter, MeterTypeId, ReconciliationMetadata, TimeRange, UsageCollectorPluginError,
    UsageCollectorPluginV1, UsageRecord,
};

#[derive(Debug, Default)]
pub struct NoopBackend;

impl NoopBackend {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self
    }

    /// The single position this backend ever issues.
    ///
    /// A `FeedPosition` may not be empty and this backend has no ordering to
    /// encode, so it issues one constant byte, at the head by construction:
    /// nothing is ever stored behind it.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::Internal`] if the byte fails to encode,
    /// which it cannot. Returned rather than asserted because this crate
    /// denies `expect`.
    fn head_position() -> Result<FeedPosition, UsageCollectorPluginError> {
        FeedPosition::new(vec![0]).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "the noop backend could not encode its constant feed position: {e}"
            ))
        })
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
        _converged_only: bool,
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

    /// An empty page, carrying a head cursor on a live read and none once a
    /// bounded replay has reached its `until`.
    ///
    /// This backend stores nothing, so it retains nothing, so every read is at
    /// the head. DESIGN §3.3's `feed-bootstrap-position` requires exactly that
    /// of a subscription retaining no entries: an empty page carrying a head
    /// cursor, never an absent one. That check is a bootstrap read, which
    /// carries no `until`.
    ///
    /// A bounded replay is the other case and answers `None`, because an
    /// absent cursor is what says a bounded replay has reached its `until`
    /// ([`FeedPage::next`]). Here that is **any** `until` at all: the only
    /// position this backend issues is the head, so a replay bounded by one
    /// has already reached it. A head cursor there would be a page a caller
    /// keeps following, which is a hang rather than a wrong value.
    ///
    /// `start` is ignored deliberately, and not merely unimplemented: this
    /// backend retains nothing, so `FeedStart::Oldest` and
    /// `FeedStart::After(head)` name the same position.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::Internal`] only if the constant position
    /// fails to encode, which it cannot.
    async fn read_feed_page(
        &self,
        _subscription: &[MeterTypeId],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition>, UsageCollectorPluginError> {
        let next = if until.is_some() {
            None
        } else {
            Some(Self::head_position()?)
        };
        Ok(FeedPage {
            entries: Vec::new(),
            next,
        })
    }

    /// Zero accepted and no watermarks, with whatever answer the declared
    /// fold has over an empty selection.
    ///
    /// This backend stores nothing, so every scope holds no entries and the
    /// count and both watermarks are the same for every caller. The fold is
    /// not: DESIGN §3.3 splits the five by whether they are defined over an
    /// empty selection, and the two halves answer differently because they
    /// are asking different things. `SUM` and `COUNT` are totals, and the
    /// total of nothing is zero — a value the caller can charge against and
    /// reconcile with. `MAX`, `MIN` and `LATEST` select an entry, and there
    /// is no entry to select, so there is nothing to report rather than a
    /// zero to report. Collapsing the two onto one answer would make a
    /// reconciling consumer unable to tell a meter that summed to zero from
    /// one whose fold could not be taken.
    ///
    /// This is why the `fold` argument is read here and nowhere else in this
    /// backend. [`ReconciliationMetadata::empty()`] supplies the rest: it is
    /// the base a plugin fills in, and this method is a plugin that knows its
    /// fold.
    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _gts_type_id: MeterTypeId,
        _time_range: TimeRange,
        fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        // Exhaustive rather than wildcarded: `AggregationFold` declares itself
        // a closed set, and a fold admitted later needs a deliberate decision
        // about which half it falls in, which a wildcard arm would make for
        // it silently.
        let quantity_summary = match fold {
            AggregationFold::Sum | AggregationFold::Count => Some(BigDecimal::from(0)),
            AggregationFold::Max | AggregationFold::Min | AggregationFold::Latest => None,
        };
        Ok(ReconciliationMetadata {
            quantity_summary,
            ..ReconciliationMetadata::empty()
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "plugin_tests.rs"]
mod plugin_tests;
