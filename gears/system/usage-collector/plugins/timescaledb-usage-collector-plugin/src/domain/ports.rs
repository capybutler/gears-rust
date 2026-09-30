use async_trait::async_trait;
use std::time::Duration;
use toolkit_odata::{ODataQuery, Page as ODataPage, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, MetadataFilter, MeterTypeId,
    TimeRange, UsageCollectorPluginError, UsageRecord,
};

/// What one feed page read observed, before the adapter turns it into a
/// `FeedPage<FeedPosition>`.
///
/// The store answers positions as `(u64, Uuid)` pairs rather than encoded
/// [`FeedPosition`](usage_collector_sdk::FeedPosition)s, so the encoding stays
/// in one module and the store stays free of it. `next` is `None` only when a
/// bounded replay reached its `until`; a live read always carries one.
#[derive(Debug)]
pub struct FeedPageRows {
    /// The page's entries, in feed order.
    pub entries: Vec<UsageRecord>,
    /// The continuation, as a position pair.
    pub next: Option<(u64, Uuid)>,
}

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
    /// One snapshot-consistent feed page in feed order, over the subscribed
    /// GTS types and inside the caller's compiled PDP scope. This is what
    /// realizes `cpt-cf-uc-plugin-fr-usage-feed`: everything the requirement
    /// asks of the feed — deterministic order, replay safety under the
    /// settled horizon, and the retention refusal below — is this method's
    /// contract.
    ///
    /// `after` is the position to continue from; `None` is a first read, which
    /// begins at the oldest entry the subscription retains and places no lower
    /// bound on position — never the head, and never refused (this plugin's
    /// `docs/DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-feed-page`, First read).
    /// `until` bounds a replay from above, inclusively.
    ///
    /// The page carries only **settled** entries: the store fixes a snapshot,
    /// reads the settled horizon under it, and bounds the page below that
    /// horizon, so no entry can later become visible at or before a returned
    /// position whatever the concurrency or commit order.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::CursorBeyondRetention`] when retention has
    /// removed an entry of a subscribed type after `after`, decided from the
    /// per-type retention marks rather than from the position's age.
    /// [`UsageCollectorPluginError::Transient`] on a retryable backend failure,
    /// and [`UsageCollectorPluginError::Internal`] otherwise.
    async fn feed_page(
        &self,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
        after: Option<(u64, Uuid)>,
        until: Option<(u64, Uuid)>,
        limit: u64,
    ) -> Result<FeedPageRows, UsageCollectorPluginError>;
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

/// Why a type's declared retention could not be resolved.
///
/// Every variant keeps data: the retention sweep never drops a chunk without a
/// definite retention for each type it may hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionError {
    /// The registry client is missing, or the call failed with anything other
    /// than a definite not-found.
    Unavailable(String),
    /// The registry holds no such type.
    NotFound,
    /// The type declares no `retention` trait.
    MissingTrait,
    /// `retention` is not a string, not a fixed-length ISO 8601 duration, or
    /// zero.
    InvalidTrait(String),
}

/// Reads the declared retention of a GTS type.
///
/// Retention is the one declaration attribute this plugin reads, because it is
/// the component that applies it (the gear's `DESIGN.md` §3.3). It is mutable,
/// so an implementation must not cache it across sweeps.
#[async_trait]
pub trait RetentionSource: Send + Sync + 'static {
    async fn retention(&self, gts_type_id: &str) -> Result<Duration, RetentionError>;
}
