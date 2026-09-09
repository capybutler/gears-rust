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
    ///
    /// **At most one invalidation per record, and this is the only place it
    /// can be enforced.** Where the entry carries an
    /// [`UsageRecord::invalidation`], the store MUST reject it if the record
    /// it names already has an accepted invalidation, and MUST make that
    /// check atomic with the entry it admits — one backend transaction, not
    /// a read followed by a write. Report the rejection as
    /// [`UsageCollectorPluginError::AlreadyInvalidated`], naming the
    /// invalidation already in place.
    ///
    /// The gateway does not pre-read for this and will not: a gateway-side
    /// check cannot exclude a concurrent second submission, so it would fail
    /// exactly when it matters
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`). Nothing
    /// upstream of this method enforces the rule, which is why an
    /// implementation that omits it is not merely permissive — it admits two
    /// withdrawals of one measurement, and the fold then excludes a pair that
    /// has three entries in it.
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError>;

    /// Persist a batch of usage records.
    ///
    /// Per-record outcomes are aligned with the input order.
    ///
    /// The at-most-one obligation on [`Self::create_usage_record`] applies to
    /// every entry here, and a batch is where it is easiest to get wrong: two
    /// withdrawals of one record can arrive in the same call, so admitting
    /// them one at a time against the state each read is not enough. Exactly
    /// one may be accepted.
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
    ///
    /// A withdrawn pair MUST be returned **as persisted** — both entries,
    /// the invalidation naming its target
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`). This is a
    /// ledger path rather than a derived view, and a plugin MUST NOT
    /// withhold a withdrawn entry from it as a kindness: hiding one
    /// destroys the audit trail the append-only model exists to keep, and
    /// the exclusion belongs to the fold instead
    /// ([`Self::query_aggregated_usage_records`]). This path carries no
    /// filter that selects withdrawn entries in or out — the target
    /// reference an invalidation carries ([`UsageRecord::invalidation`]) is
    /// what a consumer reads instead. The kind alone will not do it: an
    /// entry known to be an invalidation still has to name the record it
    /// withdrew before anything can be left out. A consumer folding entries
    /// it read here leaves a withdrawn pair out on its own side.
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
    ///
    /// **A withdrawn pair contributes nothing.** Two obligations rather
    /// than one conditional
    /// (`cpt-cf-usage-collector-adr-append-only-invalidation`), and the
    /// plugin MUST honour both:
    ///
    /// 1. An **invalidation entry** contributes nothing to any fold,
    ///    whether or not its target is in the selection.
    /// 2. A **record an accepted invalidation names** contributes nothing
    ///    either.
    ///
    /// The reference an entry carries ([`UsageRecord::invalidation`]) is
    /// both what makes it an invalidation, for the first, and what names
    /// the record to leave out with it, for the second.
    ///
    /// Leaving out only the record double-counts the measurement the
    /// withdrawal was meant to remove, because an invalidation echoes the
    /// quantity it withdraws rather than negating it. The rule holds under
    /// every value of `fold` and needs no interpretation of what a quantity
    /// means, which is what makes withdrawal expressible as one rule across
    /// every declared fold rather than one rule per meter kind.
    ///
    /// The first obligation standing alone is not pedantry. Retention is
    /// plugin-owned (DESIGN §3.10 "Consistency Contract"), so a conforming
    /// deployment can purge a target and keep the invalidation that
    /// withdrew it. That orphan still contributes nothing, and admitting it
    /// would be the double count in its purest form — the echoed quantity
    /// reported with nothing left to pair it against.
    ///
    /// Both entries carry one covered period, so no `time_range` selects
    /// one of the pair without the other and no placement of the
    /// invalidation changes a result.
    ///
    /// A materialised aggregate MUST **recompute** over the affected range
    /// rather than absorb an appended term: no further term reverses `MAX`,
    /// `MIN` or `LATEST`. Append-only is a property of the ledger, not of a
    /// derived view.
    ///
    /// This is a read-path obligation, distinct from the store's one
    /// admission-time invalidation rule
    /// ([`UsageCollectorPluginError::AlreadyInvalidated`]): that one
    /// decides what is admitted, this one what a fold counts. The gear
    /// enforces neither — it dispatches this call and returns what the
    /// plugin computes — so the `invalidation-excluded-from-fold` contract
    /// test DESIGN §3.3 "Plugin SPI" requires of every conforming plugin is
    /// what binds an implementation to it.
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
    /// A withdrawn pair MUST likewise be returned as persisted here, under
    /// the ledger obligation [`Self::get_usage_record`] states — both
    /// entries, though **not necessarily on one page**: the pair shares a
    /// `window_end` but not an `id`, and an admissible order names both, so
    /// whichever of the two it leads with, a page boundary can fall between
    /// them. A consumer leaving a pair out folds over a range it has read
    /// whole rather than over a single page.
    ///
    /// "No filter that selects them in or out" means none this method
    /// applies on its own initiative. A caller's `$filter` may name
    /// `invalidates` or `entry_type` — the filterable schema declares both,
    /// the former precisely so a consumer folding entries itself can find a
    /// withdrawn pair.
    ///
    /// `query.order` MUST be honoured: it is the keyset the page
    /// continuation is built from, so ignoring it drops rows across a page
    /// boundary. The gateway guarantees the slot is usable on every
    /// surface — REST, the in-process client, and a direct service call
    /// alike: `query.order` is non-empty, uses one sort direction
    /// throughout, names only never-null record attributes, and names both
    /// `window_end` and `id`, so the sort tuple is globally unique. A
    /// plugin therefore needs no fallback keyset of its own, and an empty
    /// order is a gateway breach rather than a case to paper over.
    ///
    /// Those two names are guaranteed to be *present*, not to be last: a
    /// caller ordering by `id` is handed on as `(id, window_end)`. A
    /// plugin MUST read the order it is given rather than assume a
    /// position for either key.
    ///
    /// The obligation runs the other way on the way out. A `next_cursor`
    /// this method mints MUST be bound to the order it was handed —
    /// `cursor.s == query.order.to_signed_tokens()` — and its boundary
    /// values MUST be one per key of that order. The gateway hands the
    /// follow-up request back with the order decoded from those signed
    /// tokens and requires it to be a sound keyset: non-empty, one
    /// direction, never-null keys, and naming both canonical fields. A
    /// token bound to anything else is refused with `toolkit_odata`'s
    /// `InvalidCursor`, which the gateway propagates rather than restates —
    /// the caller reads `INVALID_CURSOR` because upstream declares it, not
    /// because this gear does (Spec §3.13). The caller then cannot
    /// continue, so minting against a different order breaks
    /// pagination for the plugin's own pages. The gateway cannot repair it
    /// instead of refusing: appending a key would leave the order wider
    /// than the boundary values the token carries, which is a silently
    /// wrong page rather than a refused one.
    ///
    /// `query.filter_hash` is likewise guaranteed **on this method**: the
    /// gateway populates it for every `list_usage_records` dispatch —
    /// REST, the in-process client, and a direct service call alike, and on
    /// a first page as much as on a continuation — so an implementation of
    /// this method never has to handle `None`, and an absent value is a
    /// gateway breach rather than a case to paper over.
    ///
    /// The scope is deliberate and narrower than the `query.order`
    /// guarantee's. [`Self::query_aggregated_usage_records`] paginates
    /// nothing, so nothing there mints a cursor and the gateway assigns it
    /// no fingerprint: that method receives whatever `filter_hash` the
    /// caller's own surface supplied, which is `None` in process and a
    /// hash of `$filter` alone over REST. An aggregate implementation MUST
    /// NOT read the slot.
    ///
    /// A `next_cursor` MUST carry that value through verbatim as its `f`.
    /// It is the gateway's fingerprint of the query the page was read
    /// under: the caller's `$filter` together with all three typed
    /// parameters — `gts_type_id`, `time_range` and `metadata_filter` —
    /// none of which is a `$filter` conjunct, so a hash of `$filter` alone
    /// would cover none of them. The gateway recomputes the same string
    /// from the follow-up request and refuses a token carrying a different
    /// one, or none, with `toolkit_odata`'s `FilterMismatch` — which is
    /// what the caller reads as `FILTER_MISMATCH` against `cursor`. A
    /// plugin that
    /// dropped or recomputed it would therefore break pagination for its
    /// own pages, and it MUST NOT interpret the value: it is opaque, and
    /// its shape is the gateway's to change.
    async fn list_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;
}
