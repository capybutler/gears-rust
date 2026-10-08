use async_trait::async_trait;
use std::time::Duration;
use toolkit_odata::{ODataQuery, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, Keyset, MetadataFilter, MeterRef,
    ReconciliationMetadata, RecordPage, StoredUsageRecord, TimeRange, UsageCollectorPluginError,
};

/// Hard upper bound on the page size this plugin will ever serve in one read,
/// on both the ledger list paths and the feed path.
///
/// On the ledger paths this is a **defense-in-depth backstop**, not the
/// primary cap: the usage-collector core gateway already rejects `$top >
/// 1000` with `400 InvalidArgument` (its own `MAX_PAGE_SIZE`) before any
/// plugin call. The value is kept in lock-step with that gateway cap so
/// infra's clamp is never reached in normal operation — it only bites if the
/// plugin is ever driven by a different or buggy caller, preventing an
/// unbounded full-result-set read (a resource/DoS hazard) at the persistence
/// boundary.
///
/// The feed path enforces it differently: a `limit` outside `[1,
/// MAX_PAGE_SIZE]` is refused as `Internal` rather than clamped, since a
/// caller-visible page size it did not ask for would break the feed's
/// exactly-resumable-scan contract.
pub const MAX_PAGE_SIZE: u64 = 1000;

/// What one feed page read observed, before the adapter turns it into a
/// `FeedPage<FeedPosition, StoredUsageRecord>`.
///
/// The store answers positions as `(u64, Uuid)` pairs rather than encoded
/// [`FeedPosition`](usage_collector_sdk::FeedPosition)s, so the encoding stays
/// in one module. `next` is `None` only when a bounded replay reached its
/// `until`; a live read always carries one.
#[derive(Debug)]
pub struct FeedPageRows {
    /// The page's entries, in feed order.
    pub entries: Vec<StoredUsageRecord>,
    /// The continuation, as a position pair.
    pub next: Option<(u64, Uuid)>,
}

/// Persistence + query operations on `usage_records`. Implemented by infra.
#[async_trait]
pub trait RecordStore: Send + Sync + 'static {
    /// Persist a batch, each entry paired with its own meter.
    ///
    /// `meter.uuid` is the reference this ledger stores, and what
    /// `usage_type_key`, the retention marks and every meter-leading index key
    /// on. Each `record.gts_type_uuid` equals its pair's — the SPI guarantees
    /// that inbound — so the write binds the meter's and reconciles nothing.
    async fn create_batch(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>;
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
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError>;
    /// One snapshot-consistent feed page in feed order, over the subscribed
    /// GTS types and inside the caller's compiled PDP scope. Realizes
    /// `cpt-cf-uc-plugin-fr-usage-feed`: deterministic order, replay safety
    /// under the settled horizon, and the retention refusal below are all this
    /// method's contract.
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
        subscription: &[Uuid],
        scope: &ast::Expr,
        after: Option<(u64, Uuid)>,
        until: Option<(u64, Uuid)>,
        limit: u64,
    ) -> Result<FeedPageRows, UsageCollectorPluginError>;
    /// Keyset-paginated ledger read over one meter and one covered-period
    /// range.
    ///
    /// `gts_type_uuid` and `time_range` are typed parameters rather than
    /// `$filter` conjuncts, and the gateway refuses a predicate naming either
    /// covered-period bound. An entry is selected when the end of its period
    /// falls in the range, `from <= window_end < to`, whatever the length of
    /// that period (`cpt-cf-usage-collector-adr-window-end-selection`).
    ///
    /// Entries are returned as persisted: a withdrawn record and the
    /// invalidation that withdrew it both appear, because this is a ledger
    /// path rather than a derived view — though **not necessarily on one
    /// page**, since the pair shares a `window_end` but not an `id` and an
    /// admissible order names both. A consumer folding the pair out folds over
    /// a range it has read whole, not over a single page.
    async fn list(
        &self,
        gts_type_uuid: Uuid,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError>;
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
        gts_type_uuid: Uuid,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError>;
    /// Per-scope ingestion counters and watermarks over one `(tenant_id,
    /// gts_type_uuid)` scope, for the reconciliation read.
    ///
    /// `accepted_count` covers the range and keeps invalidations and
    /// withdrawn pairs; `quantity_summary` covers the same range but excludes
    /// both halves of a withdrawn pair, in the branch `fold` selects
    /// (`usage_collector_sdk::QuantitySummary::accrues`). `max_accepted_at`
    /// and `max_window_end` are unbounded by `time_range` — a range selecting
    /// nothing still reports them, and only a scope holding no entries leaves
    /// both absent.
    ///
    /// `scope`, `tenant_id` and `gts_type_uuid` are conjuncts of one `WHERE`
    /// rather than a sequence, so neither of the call's own parameters can
    /// widen what `scope` admits: a tenant it excludes answers exactly as one
    /// holding no entries, never an error — the disposition
    /// [`RecordStore::get`] and [`RecordStore::feed_page`] give an
    /// out-of-scope row.
    async fn reconciliation(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError>;
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
///
/// Keyed on the registry reference, because that is the only meter identity
/// this ledger stores: `usage_type_key` holds a `uuid`, so the sweep has no
/// identifier to pass and may not derive one.
#[async_trait]
pub trait RetentionSource: Send + Sync + 'static {
    async fn retention(&self, gts_type_uuid: Uuid) -> Result<Duration, RetentionError>;
}
