//! Storage Plugin SPI for the Usage Collector.

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, ast};
use uuid::Uuid;

use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPage, FeedPosition, FeedStart};
use crate::keyset::{Keyset, RecordPage};
use crate::models::{AggregationDimension, AggregationFold, AggregationResult, MetadataFilter};
use crate::reconciliation::ReconciliationMetadata;
use crate::stored::{MeterRef, StoredUsageRecord};
use crate::time_range::TimeRange;

/// Backend storage adapter trait implemented by
/// `usage-collector-plugin-<backend>` crates. DESIGN §3.3 names this trait
/// `cpt-cf-usage-collector-interface-plugin` and its contract
/// `cpt-cf-usage-collector-contract-storage-plugin`.
///
/// # Consistency floor parity (DESIGN §3.10)
///
/// The same floor binds this SPI; nothing an implementation does relax or
/// strengthen it. Quoted verbatim from the REST surface's own copy
/// (`usage_collector::api::rest::routes::usage_records::CONSISTENCY_FLOOR_STATEMENT`),
/// because `dod-consistency-floor-published` requires the statement be
/// worded identically wherever it is published, modulo this doc comment's
/// Markdown code-span backticks around the two field names, which carry no
/// wording of their own:
///
/// Consistency floor (DESIGN.md Section 3.10): after an ingestion call
/// returns the persisted entry, that entry is durable. Under eventual
/// dedup level, an acknowledged entry can still lose a race before
/// convergence (DESIGN.md Section 3.1, Dedup level). This read surface is
/// eventually consistent with no upper bound relative to a same-tenant
/// ingestion ack. No monotonic-reads guarantee at the floor. The floor is
/// per (`tenant_id`, `gts_type_id`). The floor claims no ordering of
/// entries. A consumer depending on a tighter bound than this floor must
/// record that dependency in its own design document, naming the plugin,
/// the dimension, and the value; weakening a published bound is a breaking
/// change for every coupled consumer, and the Plugin SPI publishes no
/// runtime method for discovering a plugin's ceiling in v1.
///
/// **Realizes the SPI half of `dod-consistency-floor-published` and of
/// `dod-staleness-coupling-recorded`** — this trait's absence of a
/// profile-advertisement method is that second identifier's other requirement.
/// The REST half of both is
/// `usage_collector::api::rest::routes::usage_records::CONSISTENCY_FLOOR_STATEMENT`,
/// and the two copies are pinned against each other by
/// `consistency_floor_tests::every_required_site_publishes_the_full_floor_statement`.
///
/// # Obligations (DESIGN §3.3)
///
/// - **Pure persistence.** Authorization, type resolution, shape and quantity
///   validation and the invalidation copy rules are the gateway's. A plugin
///   that observes a violation of one has observed a host-contract breach and
///   returns [`UsageCollectorPluginError::Internal`]; it does not re-validate.
/// - **Key on the reference, never the identifier.** A meter crosses this
///   SPI as a [`MeterRef`]: `uuid` is the identity a plugin persists,
///   indexes and partitions by, and `id` is diagnostic — loggable, never
///   persisted and never derived from `uuid` or vice versa. One identifier
///   maps to one reference for the life of an installation
///   (`cpt-cf-types-registry-adr-storage-identity-query-model`), which is
///   what makes the reference safe as a storage key.
/// - **Acknowledge only what is durable.** A persist call returns only after
///   every entry it reports accepted is durable. A plugin may buffer and
///   coalesce calls, but a buffer holds only unacknowledged entries.
/// - **Declare a dedup level and meet it** (DESIGN §3.1 "Dedup level", published
///   in the plugin's deployment guide). A collision on the dedup identity
///   resolves by [`StoredUsageRecord::caller_supplied_eq`]: equal is absorbed and
///   returns the stored entry, different is
///   [`UsageCollectorPluginError::IdempotencyConflict`] carrying it. A second
///   invalidation of one record is an ordinary collision on the dedup identity
///   every withdrawal of that record reaches, with no check of its own.
/// - **Decide converged-only lookups.** See [`Self::get_usage_record`].
///
/// The contract suite in `usage_collector_sdk::contract` (feature `contract`)
/// is what binds an implementation to these.
///
/// **Realizes the SPI half of
/// `cpt-cf-usage-collector-dod-no-gear-local-privacy-workflow`'s second
/// sentence** — "the storage plugin interface MUST carry no erasure or
/// redaction method". The methods below are this trait's entire method
/// surface and none names such an operation. The clause's other sentence (no
/// gear-local privacy-workflow surface) is realized at
/// `usage_collector::api::rest::routes::register_api_routes`. Pinned by
/// `data_classification_tests::the_storage_plugin_interface_declares_no_erasure_or_redaction_method`.
// @cpt-dod:cpt-cf-usage-collector-dod-plugin-spi-sole-seam:p1
// @cpt-dod:cpt-cf-usage-collector-dod-plugin-spi-compat-rule:p1
// @cpt-dod:cpt-cf-usage-collector-dod-no-gear-local-privacy-workflow:p3
// @cpt-dod:cpt-cf-usage-collector-dod-consistency-floor-published:p1
// @cpt-dod:cpt-cf-usage-collector-dod-staleness-coupling-recorded:p1
#[async_trait]
pub trait UsageCollectorPluginV1: Send + Sync + 'static {
    /// Persist a batch of usage records.
    ///
    /// Per-record outcomes are aligned with the input order, and each is
    /// resolved as follows.
    ///
    /// A collision on the dedup identity (and so on `id`, derived from it)
    /// resolves by comparing caller-supplied fields
    /// ([`StoredUsageRecord::caller_supplied_eq`]). An equal submission is absorbed
    /// and the **stored** entry is returned, its `accepted_at` and `origin`
    /// included. A different one is
    /// [`UsageCollectorPluginError::IdempotencyConflict`] carrying the stored
    /// entry. The rule covers invalidations too: every invalidation of one
    /// record repeats that record's tenant, type, key and period and reads
    /// `entry_type = invalidation`, so all of them share one dedup identity —
    /// a second one with the same reason code is absorbed and one with another
    /// reason code conflicts. There is no separate at-most-one check.
    ///
    /// In each pair, `record.gts_type_uuid` is always `meter.uuid` — the
    /// gateway builds every record it dispatches from the same resolved
    /// declaration as the paired reference, so the two can never disagree
    /// inbound. A plugin may key on either; the pair exists so a backend has
    /// the identifier available to log without reading it off the record.
    ///
    /// Two same-identity entries in one call resolve later against earlier:
    /// the later is absorbed when its caller-supplied fields equal the earlier
    /// accepted entry's, and conflicts otherwise. The gateway already sends one
    /// entry per identity; this is defence in depth.
    ///
    /// Each entry is paired with its own [`MeterRef`] rather than the batch
    /// carrying a separate table of them: a pair cannot fail to cover its
    /// record, so there is no coverage obligation for a plugin to breach and
    /// no missing-meter arm for one to implement.
    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>;

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
    /// reference an invalidation carries ([`StoredUsageRecord::invalidation`]) is
    /// what a consumer reads instead. The kind alone will not do it: an
    /// entry known to be an invalidation still has to name the record it
    /// withdrew before anything can be left out. A consumer folding entries
    /// it read here leaves a withdrawn pair out on its own side.
    ///
    /// **`converged_only`.** The gateway passes `true` when it resolves an
    /// invalidation's target and `false` on the caller-facing point read.
    /// With `true` the plugin applies `scope` first, returns the survivor once
    /// the identity has converged, never reports an acknowledged, retained
    /// entry missing, and answers
    /// [`UsageCollectorPluginError::UsageRecordNotConverged`] only until it can
    /// decide: within its convergence bound plus its query-path lag bound it
    /// returns the entry or [`UsageCollectorPluginError::UsageRecordNotFound`].
    /// A plugin whose every read is converged (a single linearizable primary)
    /// ignores the flag.
    ///
    /// The returned entry names its meter by reference alone. This method
    /// takes no meter, so the gateway reverse-resolves the reference to name
    /// the meter on its own surface; a plugin neither performs that
    /// resolution nor needs an identifier to answer.
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError>;

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
    /// The reference an entry carries ([`StoredUsageRecord::invalidation`]) is
    /// both what makes it an invalidation, for the first, and what names
    /// the record to leave out with it, for the second.
    ///
    /// Leaving out only the record double-counts the measurement the
    /// withdrawal was meant to remove, because an invalidation echoes the
    /// quantity it withdraws rather than negating it. The rule holds under
    /// every value of `fold` and needs no interpretation of what a quantity
    /// means.
    ///
    /// The first obligation stands alone because retention is plugin-owned
    /// (DESIGN §3.10), so a conforming deployment can purge a target and keep
    /// the invalidation that withdrew it. That orphan still contributes
    /// nothing.
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
    /// This is a read-path obligation. The gear does not enforce it — it
    /// dispatches this call and returns what the plugin computes — so the
    /// `invalidation-excluded-from-fold` contract test DESIGN §3.3 "Plugin SPI"
    /// requires of every conforming plugin is what binds an implementation to
    /// it.
    ///
    /// **What the gateway guarantees about `group_by`**, on every surface
    /// REST, the in-process client and a direct service call alike — the
    /// aggregate counterpart of the `query.order` guarantee
    /// [`Self::list_usage_records`] states:
    ///
    /// * every dimension is either one of the fixed
    ///   [`AggregationDimension`] variants or a metadata key the queried
    ///   meter's resolved declaration actually declares, checked fresh per
    ///   request against that declaration (Spec §3.11), so a plugin never
    ///   has to decide what an undeclared key means; and
    /// * **no dimension appears more than once**, by exact equality — the
    ///   `uniqueItems: true` the published `group_by` schema carries, made
    ///   good at the gateway rather than left to the backend. A repeat
    ///   would otherwise reach a `GROUP BY` as a redundant term, and the
    ///   bucket a plugin returns carries one value per `group_by` entry in
    ///   the caller's order, so a repeat would publish the same dimension
    ///   twice in one bucket.
    ///
    /// There is **no** ceiling on dimension count: arity is bounded by the
    /// admissible set, and what bounds the result is the aggregation-result
    /// limit and the mandatory `time_range`.
    async fn query_aggregated_usage_records(
        &self,
        meter: &MeterRef,
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
    /// throughout, names only never-null record attributes, names no key
    /// more than once, and names both `window_end` and `id`, so the sort
    /// tuple is globally unique. A plugin therefore needs no fallback
    /// keyset of its own, and an empty order is a gateway breach rather
    /// than a case to paper over.
    ///
    /// The no-repeat guarantee is about *budget* rather than correctness: a
    /// repeated key still renders valid SQL, but it widens the keyset — and so
    /// the minted cursor — by one signed token per repeat, and
    /// `$orderby=resource_id,resource_id,resource_id` alone breaches the
    /// published cursor length. Nothing downstream dedups an order
    /// (`ODataOrderBy::ensure_tiebreaker` *skips* a field already named rather
    /// than deduplicating it out), so a plugin may rely on the guarantee rather
    /// than dedup defensively.
    ///
    /// Those two names are guaranteed to be *present*, not to be last: a
    /// caller ordering by `id` is handed on as `(id, window_end)`. A
    /// plugin MUST read the order it is given rather than assume a
    /// position for either key.
    ///
    /// `keyset` is the continuation, and it is the gateway's to decode. A
    /// first page carries `None` and MUST be served from the start of the
    /// selected range with no seek predicate; a continuation carries the
    /// boundary values of the last row of the previous page, one per key of
    /// `query.order` and in the same sequence, in that order's single
    /// direction. A plugin builds its seek from `keyset` and `query.order`
    /// together and MUST NOT read `query.order`'s position for either
    /// canonical field — see the paragraphs above.
    ///
    /// `query.cursor` is `None` on every dispatch and MUST NOT be read.
    /// Encoding, decoding, signing or validating a wire cursor is the
    /// gateway's alone
    /// (`cpt-cf-usage-collector-dod-gateway-owned-cursor`,
    /// `cpt-cf-uc-plugin-dod-no-filter-widening-no-offset`), which is why
    /// this method returns a [`RecordPage`] carrying a [`Keyset`] rather
    /// than a page carrying a token. The gateway mints the token from that
    /// keyset and verifies its arity and direction against the order it
    /// dispatched, so a keyset of the wrong width is refused as a
    /// host-contract breach rather than served.
    ///
    /// `query.filter_hash` carries no guarantee on this method and MUST NOT
    /// be read: the gateway mints the token, holds its own fingerprint, and
    /// needs nothing from a plugin to bind a token to a query.
    ///
    /// [`Self::query_aggregated_usage_records`] paginates nothing, so it mints
    /// no cursor and is assigned no fingerprint — it receives whatever
    /// `filter_hash` the caller's surface supplied (`None` in process, a hash
    /// of `$filter` alone over REST). An aggregate implementation MUST NOT read
    /// the slot either.
    async fn list_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError>;

    /// Snapshot-consistent feed page in feed order (§3.1).
    ///
    /// `start` names where the page begins. `FeedStart::After(position)` and
    /// `until` carry back positions this plugin issued; `FeedStart::Oldest`
    /// means the oldest position this plugin still serves for `subscription`
    /// under `scope` — never the head. `FeedStart` is open, so a plugin
    /// matches it with a wildcard arm and returns `Internal(detail)` there.
    /// `scope` is the compiled PDP scope. An entry outside it is absent.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::CursorBeyondRetention`] when retention has
    /// removed an entry of a subscribed type after `start`'s position, decided
    /// from what this plugin still holds rather than from the position's age.
    /// [`UsageCollectorPluginError::Transient`] on a retryable backend
    /// failure, and [`UsageCollectorPluginError::Internal`] otherwise — which
    /// is where a host-contract breach lands: a `start` mode this plugin does
    /// not know (the wildcard arm above), a position it did not issue, or a
    /// `limit` outside the published bound.
    ///
    /// Every entry on the returned page MUST carry a `gts_type_uuid` naming
    /// one of the references in `subscription`. A page is bounded by the
    /// subscription, so an entry outside it has no meter the caller asked
    /// for; the gear refuses such a page as a host-contract breach rather
    /// than serving it.
    async fn read_feed_page(
        &self,
        subscription: &[MeterRef],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError>;

    /// Per-scope ingestion counters and watermarks.
    ///
    /// The counters cover the range; the watermarks do not.
    ///
    /// `fold` is the queried meter's **declared** fold, and it is what
    /// decides the branch of [`crate::reconciliation::QuantitySummary`] this
    /// method returns — never the caller, and never this plugin's own
    /// judgment. A summing meter (`fold` is `Sum`) reports
    /// [`crate::reconciliation::QuantitySummary::Accrued`]; every other fold
    /// reports [`crate::reconciliation::QuantitySummary::Observations`].
    /// Quantities under a non-accruing fold are not summable, which is why
    /// that second branch reports a count and one observation rather than a
    /// fold result: a `MAX` over a range answers a question about the meter,
    /// while reconciliation answers a question about how much arrived.
    /// `Observations::latest` follows DESIGN §3.1's `LATEST` total order —
    /// greatest `window_end`, then greatest `accepted_at`, then greatest
    /// `id` — regardless of the declared fold's own ordering; `MAX` and
    /// `MIN` select the branch, never the ordering within it. An empty
    /// selection is a complete answer rather than an absent one and still
    /// splits by fold: `Accrued(0)` under `Sum`, `Observations { count: 0,
    /// latest: None }` under every other fold.
    /// [`crate::reconciliation::QuantitySummary`] is deliberately not
    /// `#[non_exhaustive]` — this plugin constructs it — but a returned
    /// branch that disagrees with `fold` is a host-contract breach the gear
    /// answers with an internal error, not a caller error.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::Transient`] on a retryable backend
    /// failure, [`UsageCollectorPluginError::Internal`] otherwise. A tenant
    /// the compiled `scope` excludes answers exactly as one holding no
    /// entries, never as an error.
    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError>;
}
