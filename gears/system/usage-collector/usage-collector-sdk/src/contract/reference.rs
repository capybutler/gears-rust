//! The in-memory conforming backend the contract suite is validated
//! against.
//!
//! [`InMemoryReferencePlugin`] implements every method of
//! [`UsageCollectorPluginV1`] against a `Vec<UsageRecord>` behind a mutex,
//! honouring the DESIGN §3.1 invariants the SPI puts on the plugin:
//! dedup by caller-supplied fields (which is also at most one invalidation per record),
//! scope-gated point lookup, period-end selection, and the withdrawal
//! exclusion inside a fold.
//!
//! **It is not a production backend, and it is not an example of how to
//! write one.** It holds everything in process memory, it scans linearly,
//! it mints no `next_cursor`, and it loses every entry when the process
//! exits. What it is for is the suite's own subject: a backend known to
//! conform, so that a check reporting a violation against it is a bug in
//! the check rather than an open question about the backend. A real plugin
//! projects the same obligations into its storage engine — a `WHERE` clause,
//! a unique index, a transaction — and this module deliberately does not
//! model any of that.
//!
//! # Stated limits
//!
//! Places this backend answers something a SQL projection would not, or
//! answers less than the SPI asks for. None is a defect to fix by copying
//! around it; each is here so a reader finds it without opening every doc
//! comment.
//!
//! * **A grouping dimension a row does not carry drops the row from the
//!   fold entirely** — see `bucket_key`. A row with no `subject_ref`
//!   contributes to no bucket of a `subject_id` grouping, where naive SQL
//!   would collect those rows into a NULL group. The consequence is that
//!   grouped buckets need not sum to the ungrouped total. DESIGN says
//!   nothing about the case, and `usage-collector-v1.yaml` types every
//!   `AggregationBucket.key` item as a non-nullable `string`, so dropping
//!   may be the only answer the wire shape can carry — which is why the
//!   behaviour stands rather than being changed here.
//! * **`list_usage_records` ignores `query.order` and mints no
//!   `next_cursor`** — it serves the canonical `(window_end, id)` ascending
//!   order and one page. A real plugin owes both; the SPI's own method doc
//!   is normative for it.
//! * **`LATEST` breaks a `window_end` tie on the greatest `id`, skipping
//!   `accepted_at`** — DESIGN §3.1 declares *greatest `window_end`, then
//!   greatest `accepted_at`, then greatest `id` in byte order*, so this
//!   backend omits the middle key rather than substituting for an
//!   inexpressible one: `UsageRecord` carries all three fields and this
//!   fold could implement the rule exactly. It answers as DESIGN requires
//!   only when the tied entries share an `accepted_at`, which is when
//!   DESIGN itself falls through to `id`. `latest-tie-break` is in
//!   `UNWRITTEN_CHECKS` in the parent module, so no check reports it.
//! * **`read_feed_page` refuses no cursor** — nothing is ever purged here, so
//!   `CursorBeyondRetention` is unreachable and this backend exercises the
//!   retention refusal not at all. The page, the position and the scope gate
//!   are correct; the refusal has nothing to fire on.
//! * **A feed position is an offset, which is the one mechanism DESIGN
//!   forbids** — `read_feed_page` resumes with `.skip(from)`, so its position
//!   is a count of entries to walk past rather than a key to seek to. The
//!   *property* is DESIGN-backed and load-bearing — a position counts what a
//!   read scanned, not what it admitted, which is what makes it mean the same
//!   thing under every grant — but the *mechanism* is not: DESIGN §3.3's
//!   plugin obligations say outright that *"Offset/limit scans are forbidden
//!   on both paginated paths."* A porter copying "position = count of scanned
//!   entries" into SQL writes `OFFSET n` and inherits its cost and its
//!   skew. What a real plugin owes instead is a monotonic key it can seek on
//!   that is comparable across a whole subscription — the same key the
//!   `TimescaleDB` adapter's `read_feed_page` tells its implementer to design,
//!   its own `acceptance_sequence` being monotonic per `(tenant, meter)` only.
//! * **`SUM` over an empty selection answers absent where DESIGN requires
//!   `0`** — `fold_value` returns `None` for every fold over a bucket with no
//!   rows, and DESIGN §3.3 divides them: `SUM` and `COUNT` *"are defined over
//!   an empty selection and report `0`"* while `MAX`, `MIN` and `LATEST`
//!   report absent. `COUNT` is answered correctly here and `SUM` is not. It
//!   predates the feed and no implemented check reads it, deliberately:
//!   `invalidation-excluded-from-fold` asserts its `SUM` against the
//!   surviving entry's quantity rather than against zero, because an empty
//!   `SUM` is what a backend computing no fold at all also answers. Listing
//!   it here is the point of this section — it is a place the backend answers
//!   less than the SPI asks for.
//! * **`get_reconciliation_metadata` never reports a `quantity_summary`** —
//!   the count and both watermarks are computed and the fold is always
//!   absent. `SUM` and `COUNT` are defined over an empty selection, so
//!   absent is the wrong answer under those two rather than a narrower one.
//!   No implemented check reads the field.
//!
//! # A test-only mirror of this file exists
//!
//! `contract_mutants::MutantLedger` re-implements this backend's selection,
//! admission decision, withdrawal exclusion and page order so that two
//! deliberately non-conforming subjects can break one of them each. It is
//! test-only, it is not a switch in this file, and nothing here should be
//! written to accommodate it — but an edit to any of those four behaviours
//! is an edit that mirror may need too. Its own docs say how far the
//! contract suite pins it automatically.
//!
//! # Why not the noop plugin
//!
//! `noop-usage-collector-plugin` already implements the SPI, and it cannot
//! serve here: it persists nothing. `create_usage_record` echoes its input,
//! `get_usage_record` always answers `UsageRecordNotFound`,
//! `list_usage_records` always answers an empty page, and every fold
//! returns no buckets. It therefore fails all six behavioural checks by
//! construction — not by defect. A null backend exists so the plugin-host
//! binding resolves end-to-end in development, and answering a well-formed
//! default to every call is exactly the right behaviour for that job.
//! Nothing about the contract suite is a reason to change it; a suite whose
//! subject persists nothing has nothing to assert against, which is why
//! this second backend exists.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo, ast};
use uuid::Uuid;

use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPage, FeedPosition, FeedStart};
use crate::models::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult,
    MAX_AGGREGATION_BUCKETS, MetadataFilter, MeterTypeId, UsageRecord,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::reconciliation::ReconciliationMetadata;
use crate::time_range::TimeRange;

/// An in-memory storage backend that satisfies the Plugin SPI's stated
/// obligations. See the module docs for what it is and is not for.
#[derive(Debug, Default)]
pub struct InMemoryReferencePlugin {
    entries: Mutex<Vec<UsageRecord>>,
}

impl InMemoryReferencePlugin {
    /// Creates an empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrows the ledger, lifting a poisoned lock to a plugin error.
    ///
    /// A poisoned mutex means an earlier call panicked while holding the
    /// ledger, so its contents may be half-written. That is
    /// [`UsageCollectorPluginError::Internal`] rather than
    /// [`UsageCollectorPluginError::Transient`]: retrying cannot unpoison
    /// it.
    fn ledger(&self) -> Result<MutexGuard<'_, Vec<UsageRecord>>, UsageCollectorPluginError> {
        self.entries.lock().map_err(|_| {
            UsageCollectorPluginError::internal(
                "the reference backend's ledger lock is poisoned: an earlier call panicked while \
                 holding it",
            )
        })
    }

    /// Encodes a scanned-entry count as a feed position.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::Internal`] if the encoding is rejected. It
    /// cannot be — eight bytes is well inside the published bound — but this
    /// crate denies `expect`, and a plugin that cannot encode its own position
    /// has broken an invariant, which is what `Internal` is for.
    fn encode_position(scanned: u64) -> Result<FeedPosition, UsageCollectorPluginError> {
        FeedPosition::new(scanned.to_be_bytes().to_vec()).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "the reference backend could not encode its own feed position: {e}"
            ))
        })
    }

    /// Decodes a position this backend issued.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::Internal`] for a position this backend did
    /// not issue: a plugin owns its own position encoding, so a foreign one is
    /// a host-contract breach rather than a caller fault.
    fn decode_position(position: &FeedPosition) -> Result<u64, UsageCollectorPluginError> {
        let bytes: [u8; 8] = position.as_bytes().try_into().map_err(|_| {
            UsageCollectorPluginError::internal(format!(
                "the reference backend issues eight-byte feed positions and was handed {} bytes",
                position.len()
            ))
        })?;
        Ok(u64::from_be_bytes(bytes))
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for InMemoryReferencePlugin {
    /// Admits one entry, or reports why it was refused, under one lock acquisition.
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let mut ledger = self.ledger()?;
        admit(&mut ledger, record)
    }

    /// Admits a batch, per-entry outcomes aligned to input order.
    ///
    /// The whole batch is decided under one lock acquisition, in input order, so
    /// two same-identity entries arriving in one call are ordered against each
    /// other: the first is admitted and the second is decided against it.
    ///
    /// An empty batch answers [`UsageCollectorPluginError::Internal`], so a
    /// gear-side bug that dispatches nothing surfaces at the same place
    /// whichever backend is bound. **That is a convention shared with the
    /// noop plugin, not an SPI requirement**: `create_usage_records` states
    /// no obligation for the empty case, and two backends agreeing is not a
    /// contract. A plugin author is free to answer an empty vector
    /// instead — nothing in the suite asserts either way.
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
        let mut ledger = self.ledger()?;
        Ok(records
            .into_iter()
            .map(|record| admit(&mut ledger, record))
            .collect())
    }

    /// Point lookup under the compiled scope.
    ///
    /// A row that exists but does not satisfy `scope` is
    /// [`UsageCollectorPluginError::UsageRecordNotFound`], the same answer
    /// an absent `id` gets. There is deliberately no distinguishable
    /// "denied": telling the two apart would make this surface an existence
    /// oracle for other tenants' records.
    ///
    /// A withdrawn pair is returned as persisted. This is a ledger path, and
    /// the exclusion belongs to the fold.
    ///
    /// The in-memory ledger is always converged, so `converged_only` changes
    /// nothing.
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        ledger
            .iter()
            // A scope this backend cannot translate excludes the row rather
            // than refusing the lookup: an uninterpretable grant admits
            // nothing. That is the opposite disposition from the caller
            // filter on the read paths, and deliberately so — see the
            // `Untranslatable` type below.
            .find(|entry| entry.id == id && expr_admits(entry, scope).unwrap_or(false))
            .cloned()
            .ok_or(UsageCollectorPluginError::UsageRecordNotFound { id })
    }

    /// Folds over the selected set, with both halves of every withdrawn
    /// pair left out.
    ///
    /// Selection is the read paths' shared one: the meter, the period end,
    /// `query.filter` and `metadata_filter`. Exclusion then removes every
    /// invalidation entry and every entry an accepted invalidation names.
    /// The second set is collected from the whole ledger rather than from
    /// the selection, because an invalidation the selection or the scope
    /// leaves out has still been accepted and still withdraws its target.
    ///
    /// A `query.filter` carrying a node this backend cannot translate
    /// refuses the fold with [`UsageCollectorPluginError::Internal`] rather
    /// than folding over the rows a dropped conjunct would leave. The
    /// disposition is the opposite of the point lookup's for the same node
    /// shape: a scope that cannot be read grants nothing, a caller filter
    /// that cannot be read is refused.
    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let withdrawn = withdrawn_targets(&ledger);
        let mut rows: Vec<&UsageRecord> = Vec::new();
        for entry in ledger.iter() {
            let selected = select(entry, &gts_type_id, time_range, query, metadata_filter)
                .map_err(Untranslatable::into_plugin_error)?;
            if selected && entry.invalidation.is_none() && !withdrawn.contains(&entry.id) {
                rows.push(entry);
            }
        }
        fold_rows(fold, &rows, group_by)
    }

    /// Keyset-ordered ledger read over the selected set.
    ///
    /// A withdrawn pair is returned as persisted, both halves, under the
    /// same ledger obligation as the point lookup.
    ///
    /// A `query.filter` carrying a node this backend cannot translate
    /// refuses the read with [`UsageCollectorPluginError::Internal`]. An
    /// empty page would be a silently wrong answer to a question the caller
    /// did not ask.
    ///
    /// The page is served in the canonical `(window_end, id)` ascending
    /// order and mints no `next_cursor`; `query.order` and continuation are
    /// not honoured. That is a stated limit of this backend rather than an
    /// oversight — no implemented check reads a second page or a
    /// caller-chosen order, and a cursor minted here would have to carry a
    /// fingerprint and a signed token set no check verifies. A real plugin
    /// owes both, and the SPI's `list_usage_records` doc is normative for
    /// it.
    async fn list_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let mut items: Vec<UsageRecord> = Vec::new();
        for entry in ledger.iter() {
            if select(entry, &gts_type_id, time_range, query, metadata_filter)
                .map_err(Untranslatable::into_plugin_error)?
            {
                items.push(entry.clone());
            }
        }
        items.sort_by(|left, right| {
            left.window_end
                .cmp(&right.window_end)
                .then_with(|| left.id.cmp(&right.id))
        });
        // With no caller limit the whole selection is one page, and
        // `PageInfo.limit` reports that page's size because there is no
        // limit to report instead. It is therefore a row count in that one
        // case, which is worth knowing before copying this: the gear always
        // populates `query.limit`, so a real plugin never reaches the
        // branch and need not choose a value for it at all.
        let limit = query
            .limit
            .unwrap_or_else(|| u64::try_from(items.len()).unwrap_or(u64::MAX));
        items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(ODataPage::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit,
            },
        ))
    }

    /// A page from the ledger's own append order.
    ///
    /// A position is the count of entries already scanned, encoded as eight
    /// big-endian bytes so its bytewise ordering matches its numeric one. The
    /// ledger only ever grows by appending, so that count is a faithful point
    /// in the feed's order, and it does not widen with the subscription's
    /// breadth.
    ///
    /// The compiled `scope` gates what the page **carries**, not what the
    /// position **counts**: an entry outside it is absent from `entries` while
    /// still advancing the cursor past itself. That split is what fixes a
    /// position's **meaning** independently of the scope and the subscription
    /// it was issued under — a position denotes a prefix of the ledger, and
    /// the same prefix under every grant — so any position resumes correctly
    /// for a caller whose grant differs, delivering every later entry that
    /// grant admits and skipping none of them. It is what DESIGN §3.1 asks
    /// for, fixing a position's age by the oldest subsequent entry of a
    /// subscribed type "whether or not the reader's authorization scope admits
    /// that entry". DESIGN's "an entry outside it is absent" is about the
    /// page, and its size bound is about the ordering. An untranslatable grant
    /// admits nothing, the same disposition as the point lookup's.
    ///
    /// **Two grants are not in general handed the same position.** `limit`
    /// bounds the entries a page *admits* while the scan is unbounded, so a
    /// page that stops on the limit stops where its own grant's entries run
    /// out rather than where the ledger does: over a ledger alternating two
    /// tenants, `limit = 1` from `Oldest` hands one grant the position 1 and
    /// the other the position 2. Resumability is the property; two positions
    /// coincide only where both reads exhausted the ledger.
    ///
    /// A later slice brings this to conformance with every feed check; what is
    /// here now is a correct page and a correct cursor, not the whole
    /// obligation.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::Internal`] for a zero `limit` and for a
    /// position this backend did not issue. Both are host-contract breaches
    /// rather than caller faults: the published page limit is at least one,
    /// and a plugin owns its own position encoding.
    async fn read_feed_page(
        &self,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition>, UsageCollectorPluginError> {
        // A zero limit emits nothing and so advances the cursor past nothing,
        // which makes `next` a fixpoint: under an `until` the caller never
        // reaches it and follows the same page forever. The published limit is
        // at least one, so a zero one is a malformed call, and DESIGN answers a
        // host-contract breach that reaches the SPI with `Internal` rather
        // than with a well-formed page that cannot advance.
        if limit == 0 {
            return Err(UsageCollectorPluginError::internal(
                "read_feed_page was called with a zero limit (host-contract breach): the \
                 published page limit is at least one, and a zero-limit page cannot advance its \
                 own cursor, so a caller following it would never finish a bounded replay",
            ));
        }

        let from = match start {
            FeedStart::Oldest => 0,
            FeedStart::After(ref position) => Self::decode_position(position)?,
        };
        let upper = match until {
            Some(ref position) => Self::decode_position(position)?,
            None => u64::MAX,
        };

        let ledger = self.ledger()?;
        let mut entries = Vec::new();
        let mut cursor = from;
        for entry in ledger
            .iter()
            .skip(usize::try_from(from).unwrap_or(usize::MAX))
        {
            if cursor >= upper || u64::try_from(entries.len()).unwrap_or(u64::MAX) >= limit {
                break;
            }
            // `cursor` has already moved past this entry, whether or not the
            // subscription and the scope admit it. Counting only admitted
            // entries would make the position depend on who asked.
            cursor += 1;
            if subscription.contains(&entry.gts_type_id)
                && expr_admits(entry, scope).unwrap_or(false)
            {
                entries.push(entry.clone());
            }
        }

        let next = if until.is_some() && cursor >= upper {
            None
        } else {
            Some(Self::encode_position(cursor)?)
        };
        Ok(FeedPage { entries, next })
    }

    /// Counters and watermarks over the ledger.
    ///
    /// The compiled `scope` applies **before** the tenant and type arguments,
    /// so a tenant it excludes answers exactly as one holding no entries
    /// rather than as an error. An untranslatable grant admits nothing, the
    /// same disposition as the point lookup's.
    ///
    /// **Do not copy the three passes.** `in_scope` re-walks the ledger once
    /// per figure, which is free enough here under one held lock and in line
    /// with a backend that scans linearly anyway. A real backend computes all
    /// three in one pass — `SELECT COUNT(*) FILTER (...), MAX(accepted_at),
    /// MAX(window_end)` — and three separate round trips would be this
    /// exemplar's shape mistaken for its obligation.
    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        _fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let in_scope = || {
            ledger
                .iter()
                .filter(|e| expr_admits(e, scope).unwrap_or(false))
                .filter(|e| e.tenant_id == tenant_id && e.gts_type_id == gts_type_id)
        };

        // `contains_window_end` rather than the inlined `from <= window_end <
        // to`: its own doc says every in-process implementation calls it
        // rather than re-deriving the boundary, and `select` above already
        // does. Re-spelling it here would be a second site to move if the
        // boundary ever did.
        let accepted_count = in_scope()
            .filter(|e| time_range.contains_window_end(e.window_end))
            .count();

        // Every field is given, so no `..ReconciliationMetadata::empty()` tail:
        // `clippy::needless_update` fires on a struct update that updates
        // nothing. The fold stays `None` until a later slice, which is when a
        // check first reads it.
        Ok(ReconciliationMetadata {
            accepted_count: u64::try_from(accepted_count).unwrap_or(u64::MAX),
            quantity_summary: None,
            max_accepted_at: in_scope().map(|e| e.accepted_at).max(),
            max_window_end: in_scope().map(|e| e.window_end).max(),
        })
    }
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

/// Decides one entry against the ledger it is being admitted to.
///
/// A collision on `id` — the `UUIDv5` of the five dedup-identity attributes, so
/// a collision already means those five agree — is decided by the rest of what
/// the caller supplied ([`UsageRecord::caller_supplied_eq`]): equal is an
/// idempotent replay answering with the stored entry, different is
/// [`UsageCollectorPluginError::IdempotencyConflict`] carrying it. A second
/// invalidation of one record is this same branch: every invalidation of one
/// record derives the same `inv:<target>` key.
fn admit(
    ledger: &mut Vec<UsageRecord>,
    record: UsageRecord,
) -> Result<UsageRecord, UsageCollectorPluginError> {
    if let Some(stored) = ledger.iter().find(|entry| entry.id == record.id) {
        if stored.caller_supplied_eq(&record) {
            return Ok(stored.clone());
        }
        return Err(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            stored.clone(),
        ));
    }
    ledger.push(record.clone());
    Ok(record)
}

/// Every `UsageRecord.id` an accepted invalidation names.
fn withdrawn_targets(ledger: &[UsageRecord]) -> std::collections::BTreeSet<Uuid> {
    ledger
        .iter()
        .filter_map(|entry| entry.invalidation.as_ref().map(|inv| inv.target))
        .collect()
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// A filter node this backend cannot translate at all.
///
/// **Distinct from "the row did not satisfy the filter", and the whole
/// point of this type.** Folding the two into one `false` answers an
/// untranslatable query with an empty page, which is a silently wrong
/// result rather than a refusal — the same defect, in a second place, as
/// collapsing "cannot compare these operands" into "these operands do not
/// match" inside [`value_matches`].
///
/// Which disposition is right depends on whose expression it is, and the
/// two arrive through different parameters:
///
/// * A **compiled PDP scope** (`get_usage_record`'s `scope`) that cannot be
///   translated MUST exclude the row. A scope this backend cannot interpret
///   admitting anything is a cross-tenant read.
/// * A **caller filter** (`query.filter` on the read paths) that cannot be
///   translated MUST refuse the query. Excluding silently answers a
///   question the caller did not ask.
///
/// `node` names the offending node kind, never an operand value, so it is
/// safe in an operator log.
#[derive(Debug, Clone)]
struct Untranslatable {
    node: String,
}

impl Untranslatable {
    fn new(node: impl Into<String>) -> Self {
        Self { node: node.into() }
    }

    /// Lifts a refusal onto the SPI's error vocabulary.
    ///
    /// [`UsageCollectorPluginError::Internal`] rather than
    /// [`UsageCollectorPluginError::Transient`]: the same filter will fail
    /// the same way on every retry.
    fn into_plugin_error(self) -> UsageCollectorPluginError {
        UsageCollectorPluginError::internal(format!(
            "the dispatched filter carries {}, which this backend cannot translate; refusing the \
             query rather than answering it with the rows a dropped conjunct would leave",
            self.node
        ))
    }
}

/// Whether one entry is inside a read path's selection.
///
/// Period selection is [`TimeRange::contains_window_end`] — the reference
/// spelling of `from <= window_end < to`. Nothing here reads
/// `window_start`, and nothing matches by overlap or containment.
///
/// The filter is evaluated before the cheap predicates so that an
/// untranslatable node is reported for any entry in the ledger rather than
/// only for one already inside the meter and the range. It is still a
/// per-entry walk, so an **empty** ledger reports nothing — which is sound:
/// with no entries the answer is the empty page whatever the filter says,
/// so there is no wrong answer to give.
fn select(
    entry: &UsageRecord,
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> Result<bool, Untranslatable> {
    let filter_admits = match query.filter() {
        Some(filter) => expr_admits(entry, filter)?,
        None => true,
    };
    Ok(filter_admits
        && entry.gts_type_id == *gts_type_id
        && time_range.contains_window_end(entry.window_end)
        && metadata_admits(entry, metadata_filter))
}

/// Whether an entry satisfies every metadata filter in the slice.
///
/// AND across the slice, including two entries naming the same key; OR
/// within one entry's values; an empty slice imposes nothing. Merging
/// same-key entries would widen the result set, which is why this iterates
/// them rather than grouping them.
fn metadata_admits(entry: &UsageRecord, filters: &[MetadataFilter]) -> bool {
    filters.iter().all(|filter| {
        entry
            .metadata
            .get(filter.key())
            .is_some_and(|value| filter.values().iter().any(|candidate| candidate == value))
    })
}

/// One record attribute, in a form comparable against an [`ast::Value`].
#[derive(Debug, Clone, Copy)]
enum FieldValue<'a> {
    Uuid(Uuid),
    Str(&'a str),
}

/// The outcome of resolving a filter identifier against one entry.
///
/// [`Self::Null`] and [`Self::Unknown`] both stop a comparison, and they
/// are different stops: an attribute this backend knows about that this row
/// happens not to carry is SQL's NULL, so the row is simply not selected;
/// an identifier outside the vocabulary is a filter that cannot be
/// translated at all.
#[derive(Debug, Clone, Copy)]
enum FieldLookup<'a> {
    /// The entry carries a value for this identifier.
    Present(FieldValue<'a>),
    /// A known identifier the entry carries no value for.
    Null,
    /// An identifier outside this backend's vocabulary.
    Unknown,
}

/// Whether an entry satisfies a filter expression.
///
/// The expression is either the gear's compiled PDP scope — a disjunction
/// of conjunctions over the five attribution identifiers — or that scope
/// AND-ed with whatever the caller's `$filter` contributed, so this
/// evaluator serves both.
///
/// **`Ok(false)` and `Err` are different answers, and the caller decides
/// what each means.** `Ok(false)` is "this row is not selected". `Err` is
/// "this expression cannot be translated", and its two dispositions are on
/// [`Untranslatable`]: a scope excludes the row, a caller filter refuses
/// the query. A plugin projecting to SQL owes the same split — an
/// untranslatable predicate is a refused query, never a dropped conjunct,
/// and an untranslatable scope grants nothing.
///
/// [`ast::Expr::Not`] is deliberately untranslatable. It is not hard to
/// evaluate; it is unsafe to, because negating a subtree that answered
/// `false` only because it could not be read turns a fail-closed answer
/// into `true` — and this evaluator can no longer even reach that mistake,
/// since an unreadable subtree is now an `Err` there is nothing to negate.
/// Nothing in the gear emits one: `scope_to_odata_filter` builds `And`,
/// `Or`, `Compare(_, Eq, _)` and `In` and nothing else.
///
/// `Compare(_, Ne, _)` is the other negation context, and it *is*
/// translated — the grammar this evaluator serves includes it, and
/// `query.filter` is caller-supplied rather than restricted to what the
/// gear compiles. It is safe only because [`value_matches`] reports "cannot
/// compare" as an outcome of its own; see [`compare_admits`].
///
/// Both operands of `And` and `Or` are evaluated rather than
/// short-circuited, so whether an expression is translatable does not
/// depend on which row it met first.
fn expr_admits(entry: &UsageRecord, expr: &ast::Expr) -> Result<bool, Untranslatable> {
    match expr {
        ast::Expr::And(left, right) => {
            let left = expr_admits(entry, left)?;
            let right = expr_admits(entry, right)?;
            Ok(left && right)
        }
        ast::Expr::Or(left, right) => {
            let left = expr_admits(entry, left)?;
            let right = expr_admits(entry, right)?;
            Ok(left || right)
        }
        ast::Expr::Compare(left, op, right) => compare_admits(entry, left, *op, right),
        ast::Expr::In(left, candidates) => in_admits(entry, left, candidates),
        ast::Expr::Not(_) => Err(Untranslatable::new("a `not` node")),
        ast::Expr::Function(name, _) => Err(Untranslatable::new(format!("the function `{name}`"))),
        ast::Expr::Identifier(name) => Err(Untranslatable::new(format!(
            "the bare identifier `{name}` in a boolean position"
        ))),
        ast::Expr::Value(_) => Err(Untranslatable::new("a bare literal in a boolean position")),
    }
}

/// Evaluates `<identifier> eq|ne <value>`.
///
/// **`ne` is a negation context.** Everything that stops a comparison has
/// to say whether it stopped because the row did not match or because the
/// comparison could not be made, or the negation turns the second into an
/// admission. The three stops:
///
/// * **A known attribute this row does not carry** — no `subject_ref`, no
///   `invalidates` on an ordinary measurement. [`FieldLookup::Null`], and
///   `Ok(false)` under both operators, matching SQL three-valued logic
///   where a comparison against NULL is NULL and the row is not selected.
///   So `subject_id ne 'x'` on a subject-less row is `Ok(false)`, not
///   `Ok(true)`.
/// * **An identifier outside the vocabulary** — [`FieldLookup::Unknown`],
///   and untranslatable.
/// * **A pairing that cannot be compared** — [`value_matches`] answering
///   `None`, and untranslatable.
///
/// Ordering operators (`gt`, `ge`, `lt`, `le`) and any operand shape other
/// than `<identifier> <op> <literal>` are untranslatable too.
fn compare_admits(
    entry: &UsageRecord,
    left: &ast::Expr,
    op: ast::CompareOperator,
    right: &ast::Expr,
) -> Result<bool, Untranslatable> {
    let (ast::Expr::Identifier(name), ast::Expr::Value(value)) = (left, right) else {
        return Err(Untranslatable::new(
            "a comparison that is not `<identifier> <op> <literal>`",
        ));
    };
    let negated = match op {
        ast::CompareOperator::Eq => false,
        ast::CompareOperator::Ne => true,
        ast::CompareOperator::Gt
        | ast::CompareOperator::Ge
        | ast::CompareOperator::Lt
        | ast::CompareOperator::Le => {
            return Err(Untranslatable::new(format!(
                "an ordering comparison on `{name}`"
            )));
        }
    };
    match record_field(entry, name) {
        FieldLookup::Unknown => Err(Untranslatable::new(format!("the identifier `{name}`"))),
        FieldLookup::Null => Ok(false),
        FieldLookup::Present(field) => match value_matches(field, value) {
            // `matched != negated` is `matched` under `eq` and `!matched`
            // under `ne`, and it is reached only for a comparison that was
            // actually made.
            Some(matched) => Ok(matched != negated),
            None => Err(Untranslatable::new(format!(
                "a comparison of `{name}` against {}",
                value_kind(value)
            ))),
        },
    }
}

/// Evaluates `<identifier> in (<value>, …)`.
///
/// Every candidate is examined rather than short-circuited on the first
/// hit, so an untranslatable candidate is reported whether or not an
/// earlier one matched. Dropping it instead would narrow the disjunction —
/// safe for a scope, and a silently different question for a caller filter.
fn in_admits(
    entry: &UsageRecord,
    left: &ast::Expr,
    candidates: &[ast::Expr],
) -> Result<bool, Untranslatable> {
    let ast::Expr::Identifier(name) = left else {
        return Err(Untranslatable::new(
            "an `in` whose left operand is not an identifier",
        ));
    };
    let field = match record_field(entry, name) {
        FieldLookup::Unknown => {
            return Err(Untranslatable::new(format!("the identifier `{name}`")));
        }
        FieldLookup::Null => return Ok(false),
        FieldLookup::Present(field) => field,
    };
    let mut matched = false;
    for candidate in candidates {
        let ast::Expr::Value(value) = candidate else {
            return Err(Untranslatable::new(format!(
                "a non-literal candidate in `{name} in (...)`"
            )));
        };
        match value_matches(field, value) {
            Some(hit) => matched |= hit,
            None => {
                return Err(Untranslatable::new(format!(
                    "a candidate of {} in `{name} in (...)`",
                    value_kind(value)
                )));
            }
        }
    }
    Ok(matched)
}

/// Resolves a filter identifier to the entry's value for it.
///
/// The vocabulary is `UsageRecordQuery`'s, minus the two covered-period
/// bounds. Those are reserved on the `$filter` surface — the range travels
/// as a typed parameter and the gear refuses a predicate naming either — so
/// they are [`FieldLookup::Unknown`] here, which refuses a caller filter
/// naming one instead of answering it, and it keeps a `chrono` comparison
/// out of a crate that carries no `chrono` dependency.
fn record_field<'a>(entry: &'a UsageRecord, name: &str) -> FieldLookup<'a> {
    let optional = |value: Option<FieldValue<'a>>| match value {
        Some(value) => FieldLookup::Present(value),
        None => FieldLookup::Null,
    };
    match name {
        "id" => FieldLookup::Present(FieldValue::Uuid(entry.id)),
        "tenant_id" => FieldLookup::Present(FieldValue::Uuid(entry.tenant_id)),
        "resource_id" => FieldLookup::Present(FieldValue::Str(entry.resource_ref.resource_id())),
        "resource_type" => {
            FieldLookup::Present(FieldValue::Str(entry.resource_ref.resource_type()))
        }
        "subject_id" => optional(
            entry
                .subject_ref
                .as_ref()
                .map(|subject| FieldValue::Str(subject.subject_id())),
        ),
        "subject_type" => optional(
            entry
                .subject_ref
                .as_ref()
                .and_then(|subject| subject.subject_type().map(FieldValue::Str)),
        ),
        "invalidates" => optional(
            entry
                .invalidation
                .as_ref()
                .map(|inv| FieldValue::Uuid(inv.target)),
        ),
        "entry_type" => FieldLookup::Present(FieldValue::Str(entry.entry_type().as_str())),
        "origin" => FieldLookup::Present(FieldValue::Str(entry.origin.as_str())),
        _ => FieldLookup::Unknown,
    }
}

/// Names a literal's type for a diagnostic, never its value.
///
/// [`ast::Value`]'s own `Display` happens to render exactly this, and this
/// function exists anyway: a diagnostic and a comparison want opposite
/// things from a literal, and having one helper for the naming keeps
/// [`value_matches`] from ever reaching for `Display` because it was
/// convenient.
fn value_kind(value: &ast::Value) -> &'static str {
    match value {
        ast::Value::Null => "a null literal",
        ast::Value::Bool(_) => "a boolean literal",
        ast::Value::Number(_) => "a numeric literal",
        ast::Value::Uuid(_) => "a UUID literal",
        ast::Value::DateTime(_) => "a date-time literal",
        ast::Value::Date(_) => "a date literal",
        ast::Value::Time(_) => "a time literal",
        ast::Value::String(_) => "a string literal",
    }
}

/// Compares a record attribute against a literal from the filter.
///
/// **Never route this through [`ast::Value`]'s `Display`.** That impl
/// prints the *type name* — `"uuid"`, `"string"`, `"number"` — not the
/// value, so a comparison built on it would find every UUID equal to every
/// other and produce a filter matching every row. `tenant_id` compiles to
/// [`ast::Value::Uuid`] while the other four identifiers compile to
/// [`ast::Value::String`], so both spellings have to be matched explicitly.
///
/// The cross-typed pairs mirror the gear's own coercion policy
/// (`authz::coerce_scope_value`): a UUID-typed field accepts a UUID-shaped
/// string, and a string-typed field accepts a UUID rendered canonically, so
/// a `resource_id` that happens to be a UUID matches however the PEP
/// compiler typed it.
///
/// **`None` means the pairing cannot be compared, and it is a distinct
/// answer from `Some(false)`.** Collapsing the two into one `bool` reads
/// correctly under `eq` and inverts under `ne`, where an uncomparable
/// pairing would come back `true` and admit the row. A UUID-typed field
/// against a string that does not parse as a UUID is the pairing that
/// matters most, because the gear denies exactly that mismatch
/// (`authz::scope_value_to_ast` lifts it to `AuthorizationDenied`), so
/// answering anything but "cannot compare" would leave this backend more
/// permissive than the gear whose scopes it models.
fn value_matches(field: FieldValue<'_>, value: &ast::Value) -> Option<bool> {
    match (field, value) {
        (FieldValue::Uuid(left), ast::Value::Uuid(right)) => Some(left == *right),
        // `Some(false)` when the string is a UUID that differs, `None` when
        // it is not a UUID at all: a genuine non-match and an uncomparable
        // operand, kept apart.
        (FieldValue::Uuid(left), ast::Value::String(right)) => {
            Uuid::parse_str(right).ok().map(|right| left == right)
        }
        (FieldValue::Str(left), ast::Value::String(right)) => Some(left == right),
        (FieldValue::Str(left), ast::Value::Uuid(right)) => Some(left == right.to_string()),
        // `Number`, `Bool`, `Null`, `DateTime`, `Date` and `Time` against
        // either field kind: no comparison this backend can make.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// Groups the selected rows and folds each group.
///
/// An empty `group_by` is the no-grouping case: exactly one bucket carrying
/// an empty key, emitted even over an empty selection — `COUNT` answers
/// zero there and every other fold answers absent, the split
/// `SELECT COUNT(*)` makes against `SELECT MIN(v)` over no rows.
///
/// The bucket count is capped at [`MAX_AGGREGATION_BUCKETS`] `+ 1`. The SDK
/// puts that bound on the plugin rather than on the gateway so an
/// unbounded grouping cannot materialise in plugin memory; one bucket past
/// the cap is what lets the gateway tell "at the cap" from "over it" and
/// refuse the latter.
fn fold_rows(
    fold: AggregationFold,
    rows: &[&UsageRecord],
    group_by: &[AggregationDimension],
) -> Result<AggregationResult, UsageCollectorPluginError> {
    if group_by.is_empty() {
        return Ok(AggregationResult {
            buckets: vec![AggregationBucket {
                key: Vec::new(),
                value: fold_value(fold, rows)?,
            }],
        });
    }

    let mut groups: BTreeMap<Vec<String>, Vec<&UsageRecord>> = BTreeMap::new();
    for row in rows {
        if let Some(key) = bucket_key(row, group_by) {
            groups.entry(key).or_default().push(row);
        }
    }

    let mut buckets = Vec::with_capacity(groups.len().min(MAX_AGGREGATION_BUCKETS + 1));
    for (key, group) in groups.into_iter().take(MAX_AGGREGATION_BUCKETS + 1) {
        buckets.push(AggregationBucket {
            key,
            value: fold_value(fold, &group)?,
        });
    }
    Ok(AggregationResult { buckets })
}

/// The key one row contributes, or `None` when a dimension is absent on it.
///
/// A row with no subject is excluded from a `subject_id` grouping rather
/// than bucketed under an empty string, and the same holds for
/// `subject_type` and for a metadata key the row does not carry. This
/// diverges from a naive SQL `GROUP BY`, and the module docs' "Stated
/// limits" argue why it stands.
fn bucket_key(row: &UsageRecord, group_by: &[AggregationDimension]) -> Option<Vec<String>> {
    group_by
        .iter()
        .map(|dimension| match dimension {
            // `Uuid::to_string()` — lowercase and hyphenated — is the key
            // encoding DESIGN §3.3 fixes for this dimension.
            AggregationDimension::TenantId => Some(row.tenant_id.to_string()),
            AggregationDimension::ResourceId => Some(row.resource_ref.resource_id().to_owned()),
            AggregationDimension::ResourceType => Some(row.resource_ref.resource_type().to_owned()),
            AggregationDimension::SubjectId => row
                .subject_ref
                .as_ref()
                .map(|subject| subject.subject_id().to_owned()),
            AggregationDimension::SubjectType => row
                .subject_ref
                .as_ref()
                .and_then(|subject| subject.subject_type().map(ToOwned::to_owned)),
            AggregationDimension::Metadata(key) => row.metadata.get(key).cloned(),
        })
        .collect()
}

/// Applies one fold to one bucket's rows.
///
/// Accumulation is in [`BigDecimal`] rather than [`Decimal`]: a wide `SUM`
/// of in-range quantities can exceed `Decimal`'s ~7.9×10²⁸ ceiling, and the
/// aggregate surface carries `BigDecimal` for exactly that reason.
///
/// `LATEST` breaks a `window_end` tie on the greatest `id`, and **that is
/// not the declared rule.** DESIGN §3.1 declares greatest `window_end`,
/// then greatest `accepted_at`, then greatest `id` in byte order. The key
/// below is `(window_end, id)`, so it skips `accepted_at` and agrees with
/// DESIGN only where the tied entries share one — the case in which DESIGN
/// also reaches `id`. Nothing makes this unavoidable: [`UsageRecord`]
/// carries `accepted_at`, so this is an omission rather than a substitute
/// for something the type cannot express, and the earlier claim here that
/// the declared tie-break read an `acceptance_sequence` field described a
/// rule DESIGN does not carry. What `id` does buy is a total order, without
/// which the answer would depend on ledger insertion order.
///
/// `latest-tie-break` is writable against the current SPI and unwritten, so
/// it sits in `UNWRITTEN_CHECKS` in the parent module and no check asserts
/// this either way. Being the suite's reference backend, this fold is not
/// evidence of what the rule is: a plugin author reads
/// [`AggregationFold::Latest`](crate::models::AggregationFold::Latest) and
/// DESIGN §3.1 for that.
fn fold_value(
    fold: AggregationFold,
    rows: &[&UsageRecord],
) -> Result<Option<BigDecimal>, UsageCollectorPluginError> {
    if matches!(fold, AggregationFold::Count) {
        let count = u64::try_from(rows.len()).map_err(|_| {
            UsageCollectorPluginError::internal("bucket cardinality does not fit a count")
        })?;
        return Ok(Some(BigDecimal::from(count)));
    }
    let winner = match fold {
        AggregationFold::Sum => {
            let mut total = BigDecimal::from(0);
            for row in rows {
                total += to_big_decimal(row.quantity.as_decimal())?;
            }
            return Ok(if rows.is_empty() { None } else { Some(total) });
        }
        AggregationFold::Max => rows.iter().max_by_key(|row| row.quantity.as_decimal()),
        AggregationFold::Min => rows.iter().min_by_key(|row| row.quantity.as_decimal()),
        AggregationFold::Latest => rows.iter().max_by_key(|row| (row.window_end, row.id)),
        AggregationFold::Count => None,
    };
    winner
        .map(|row| to_big_decimal(row.quantity.as_decimal()))
        .transpose()
}

/// Widens a record's quantity to the aggregate surface's carrier.
///
/// The conversion goes through the decimal rendering, which is exact for
/// every `Decimal`: both types are base-ten, so no scale is invented and
/// none is lost.
fn to_big_decimal(value: Decimal) -> Result<BigDecimal, UsageCollectorPluginError> {
    BigDecimal::from_str(&value.to_string()).map_err(|err| {
        UsageCollectorPluginError::internal(format!(
            "a persisted quantity could not be widened to the aggregate carrier: {err}"
        ))
    })
}
