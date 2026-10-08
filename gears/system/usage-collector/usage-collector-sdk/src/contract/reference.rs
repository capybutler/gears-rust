//! The in-memory conforming backend the contract suite is validated
//! against.
//!
//! [`InMemoryReferencePlugin`] implements every method of
//! [`UsageCollectorPluginV1`] against a sequence-stamped ledger behind a
//! mutex, honouring the invariants the SPI puts on the plugin: dedup by
//! caller-supplied fields (hence at most one invalidation per record),
//! scope-gated point lookup, period-end selection, and the withdrawal
//! exclusion inside a fold.
//!
//! **It is not a production backend, and it is not an example of how to write
//! one.** It holds everything in process memory, scans linearly, and loses
//! every entry when the process exits. It exists to be the suite's own
//! subject: a backend known to conform, so a check reporting a violation
//! against it is a bug in the check rather than an open question about the
//! backend. A real plugin projects the same obligations into its storage
//! engine — a `WHERE` clause, a unique index, a transaction — and this module
//! deliberately models none of that.
//!
//! # Stated limits
//!
//! Places this backend answers something a SQL projection would not. None is
//! a defect to fix by coding around it.
//!
//! * **A grouping dimension a row does not carry drops the row from the fold
//!   entirely** — see `bucket_key`. A row with no `subject_ref` contributes to
//!   no bucket of a `subject_id` grouping, where naive SQL would collect those
//!   rows into a NULL group, so grouped buckets need not sum to the ungrouped
//!   total. That is the specified behaviour, not this backend's choice: an
//!   entry carrying no value at a selected dimension is excluded from the
//!   grouping rather than bucketed under an absent value.
//!
//! # A mirror of this file exists
//!
//! `contract_mutants::MutantLedger` re-implements this backend so that
//! deliberately non-conforming subjects can break one rule each while the rest
//! of what they do is this file's. It mirrors most of this file: the selection
//! predicate and the covered-period bound it meets, the admission decision,
//! the withdrawal exclusion, the ledger page order with its seek and keyset,
//! the fold — its grouping, its bucket cap, **and the value each fold reports,
//! the empty-selection split included** — the feed page with its sequence
//! stamping, seek, scanned-entry cursor and retention refusal, and the
//! reconciliation counters and watermarks.
//!
//! The mirror is not a switch in this file and nothing here should be written
//! to accommodate it, but an edit to any mirrored behaviour is an edit that
//! mirror may need too. Its own docs say how far the contract suite pins it
//! automatically, and which behaviours no check pins yet.
//!
//! # Why this backend destructures `MeterRef { uuid, id: _ }`
//!
//! The SPI says a plugin MUST NOT persist or key on
//! [`MeterRef::id`](crate::MeterRef::id), and no type can enforce that: the
//! field has to be readable for a plugin to log it. The strongest statement
//! available is a reference implementation that never reads it, spelled so
//! that reading it is a visible diff. Every entry point below destructures the
//! parameter and discards the identifier by name — except the write method,
//! which discards the whole `MeterRef { uuid: _, id: _ }`, since
//! `create_usage_records` keys admission on the record's own `gts_type_uuid`
//! (which the SPI guarantees equals `meter.uuid`). The read and fold methods
//! have no record of their own to key on instead.
//!
//! # Why not the noop plugin
//!
//! `noop-usage-collector-plugin` persists nothing: `create_usage_records`
//! echoes its input, `get_usage_record` always answers `UsageRecordNotFound`,
//! `list_usage_records` always answers an empty page, and every fold returns
//! no buckets. It therefore fails every behavioural check [`super::run_all`]
//! runs, by construction rather than by defect — each either reads an entry
//! back or requires a collision outcome. That null backend exists so the
//! plugin-host binding resolves end-to-end in development, which is why this
//! second one exists rather than changing it.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use time::format_description::well_known::Rfc3339;
use toolkit_odata::{ODataQuery, SortDir, ast};
use uuid::Uuid;

use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPage, FeedPosition, FeedStart};
use crate::keyset::{Keyset, RecordPage};
use crate::models::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult,
    MAX_AGGREGATION_BUCKETS, MetadataFilter,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::reconciliation::{ObservedQuantity, QuantitySummary, ReconciliationMetadata};
use crate::stored::{MeterRef, StoredUsageRecord};
use crate::time_range::TimeRange;

use super::retention::ContractRetention;

/// One admitted entry and the sequence the feed orders it by.
#[derive(Debug)]
struct Entry {
    /// The sequence stamped when this entry was admitted. Unique across the
    /// ledger's whole life, and ascending in admission order.
    sequence: u64,
    /// The entry as persisted.
    record: StoredUsageRecord,
}

/// The ledger and its sequence counter.
#[derive(Debug, Default)]
struct Ledger {
    /// Admitted entries in admission order, so their sequences ascend.
    entries: Vec<Entry>,
    /// The highest sequence stamped so far. It only ever rises, and a
    /// sequence is never reissued.
    ///
    /// Held apart from `entries.len()` deliberately: a length renumbers every
    /// entry after one that is removed, and an already-issued position has to
    /// go on denoting the same point in the feed's order.
    stamped: u64,
    /// Per meter, the highest sequence retention has removed — the analogue of
    /// the `TimescaleDB` plugin's `usage_feed_retention_marks`. A mark only
    /// ever rises, and a meter that has lost nothing has no mark.
    retention_marks: BTreeMap<Uuid, u64>,
}

impl Ledger {
    /// Stamps one record with the next sequence and appends it.
    fn push(&mut self, record: StoredUsageRecord) {
        self.stamped = self.stamped.saturating_add(1);
        self.entries.push(Entry {
            sequence: self.stamped,
            record,
        });
    }

    /// The admitted records in admission order. Everything but
    /// `read_feed_page` reads the ledger through this; only the feed reads the
    /// sequences, because only it has a position to seek to.
    fn records(&self) -> impl Iterator<Item = &StoredUsageRecord> {
        self.entries.iter().map(|entry| &entry.record)
    }

    /// Removes every entry of `gts_type_uuid` whose covered period ends
    /// before `floor`, raising that meter's retention mark to the highest
    /// sequence removed.
    ///
    /// The bound is exclusive: an entry whose period ends exactly at the floor
    /// is inside the retention the floor expresses and stays. The mark is
    /// raised rather than assigned, so a later drop at a lower floor cannot
    /// lower it.
    fn drop_before(&mut self, gts_type_uuid: Uuid, floor: time::OffsetDateTime) {
        let mut highest_removed: Option<u64> = None;
        self.entries.retain(|entry| {
            let removed =
                entry.record.gts_type_uuid == gts_type_uuid && entry.record.window_end < floor;
            if removed {
                highest_removed =
                    Some(highest_removed.map_or(entry.sequence, |seen| seen.max(entry.sequence)));
            }
            !removed
        });

        if let Some(highest_removed) = highest_removed {
            let mark = self.retention_marks.entry(gts_type_uuid).or_default();
            *mark = (*mark).max(highest_removed);
        }
    }

    /// Whether retention has removed an entry of a subscribed type strictly
    /// after `position`.
    ///
    /// The comparison is strict: a position naming the highest sequence a type
    /// has lost has lost nothing *after* itself, so its continuation is intact
    /// and refusing it would refuse a cursor that must be served. Neither the
    /// caller's scope nor the position's age is an input — the mark records
    /// what was removed, not what a grant would have delivered.
    fn retention_has_passed(&self, subscription: &[MeterRef], position: u64) -> bool {
        subscription.iter().any(|MeterRef { uuid, id: _ }| {
            self.retention_marks
                .get(uuid)
                .is_some_and(|mark| *mark > position)
        })
    }
}

/// An in-memory storage backend that satisfies the Plugin SPI's stated
/// obligations. See the module docs for what it is and is not for.
#[derive(Debug, Default)]
pub struct InMemoryReferencePlugin {
    entries: Mutex<Ledger>,
}

impl InMemoryReferencePlugin {
    /// Creates an empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrows the ledger, lifting a poisoned lock to
    /// [`UsageCollectorPluginError::Internal`] rather than
    /// [`Transient`](UsageCollectorPluginError::Transient): the contents may
    /// be half-written, and retrying cannot unpoison it.
    fn ledger(&self) -> Result<MutexGuard<'_, Ledger>, UsageCollectorPluginError> {
        self.entries.lock().map_err(|_| {
            UsageCollectorPluginError::internal(
                "the reference backend's ledger lock is poisoned: an earlier call panicked while \
                 holding it",
            )
        })
    }

    /// Encodes the sequence of the last entry scanned as a feed position.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::Internal`] if the encoding is rejected. It
    /// cannot be — eight bytes is well inside the published bound — but this
    /// crate denies `expect`.
    fn encode_position(sequence: u64) -> Result<FeedPosition, UsageCollectorPluginError> {
        FeedPosition::new(sequence.to_be_bytes().to_vec()).map_err(|e| {
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
    /// Admits a batch, per-entry outcomes aligned to input order.
    ///
    /// The whole batch is decided under one lock acquisition, in input order, so
    /// two same-identity entries arriving in one call are ordered against each
    /// other: the first is admitted and the second is decided against it.
    ///
    /// An empty batch answers [`UsageCollectorPluginError::Internal`], so a
    /// gear-side bug that dispatches nothing surfaces at the same place
    /// whichever backend is bound. **That is a convention shared with the noop
    /// plugin, not an SPI requirement**: `create_usage_records` states no
    /// obligation for the empty case, and a plugin author is free to answer an
    /// empty vector instead.
    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        if records.is_empty() {
            return Err(UsageCollectorPluginError::internal(
                "create_usage_records called with an empty batch (host-contract breach)",
            ));
        }
        let mut ledger = self.ledger()?;
        Ok(records
            .into_iter()
            .map(|(MeterRef { uuid: _, id: _ }, record)| admit(&mut ledger, record))
            .collect())
    }

    /// Point lookup under the compiled scope.
    ///
    /// A row that exists but does not satisfy `scope` is
    /// [`UsageCollectorPluginError::UsageRecordNotFound`], the same answer an
    /// absent `id` gets: a distinguishable "denied" would make this surface an
    /// existence oracle for other tenants' records.
    ///
    /// A withdrawn pair is returned as persisted — this is a ledger path, and
    /// the exclusion belongs to the fold. The in-memory ledger is always
    /// converged, so `converged_only` changes nothing.
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        _converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        ledger
            .records()
            // An untranslatable scope excludes the row rather than refusing
            // the lookup: an uninterpretable grant admits nothing. The
            // opposite disposition from the caller filter on the read paths —
            // see `Untranslatable` below.
            .find(|entry| entry.id == id && expr_admits(entry, scope).unwrap_or(false))
            .cloned()
            .ok_or(UsageCollectorPluginError::UsageRecordNotFound { id })
    }

    /// Folds over the selected set, with both halves of every withdrawn
    /// pair left out.
    ///
    /// Selection is the read paths' shared one: the meter, the period end,
    /// `query.filter` and `metadata_filter`. Exclusion then removes every
    /// invalidation entry and every entry an accepted invalidation names. The
    /// second set is collected from the whole ledger rather than from the
    /// selection, because an invalidation the selection or the scope leaves
    /// out has still been accepted and still withdraws its target.
    ///
    /// An untranslatable `query.filter` refuses the fold with
    /// [`UsageCollectorPluginError::Internal`] rather than folding over the
    /// rows a dropped conjunct would leave — the opposite disposition from the
    /// point lookup's for the same node shape.
    async fn query_aggregated_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let MeterRef { uuid, id: _ } = meter;
        let ledger = self.ledger()?;
        let withdrawn = withdrawn_targets(&ledger);
        let mut rows: Vec<&StoredUsageRecord> = Vec::new();
        for entry in ledger.records() {
            let selected = select(entry, *uuid, time_range, query, metadata_filter)
                .map_err(Untranslatable::into_plugin_error)?;
            if selected && entry.invalidation.is_none() && !withdrawn.contains(&entry.id) {
                rows.push(entry);
            }
        }
        fold_rows(fold, &rows, group_by)
    }

    /// Keyset-ordered ledger read over the selected set.
    ///
    /// A withdrawn pair is returned as persisted, both halves, under the same
    /// ledger obligation as the point lookup. An untranslatable `query.filter`
    /// refuses the read with [`UsageCollectorPluginError::Internal`]; an empty
    /// page would be a silently wrong answer to a question the caller did not
    /// ask.
    ///
    /// The page is served in `query.order` — the gateway guarantees it is
    /// non-empty, uniform in direction and names only
    /// [`crate::models::is_keyset_safe_record_field`] attributes, so a stable
    /// sort over its keys in sequence reproduces a SQL `ORDER BY`. `keyset` is
    /// the continuation: `None` seeks nothing, `Some` seeks past its boundary
    /// values under the same order, compared as a typed value parsed back from
    /// the boundary string rather than as the rendering this backend returns
    /// the `Keyset` with. The look-ahead is one row past the caller's limit,
    /// trimmed before the page is returned and used only to decide whether
    /// `next` is `Some`.
    async fn list_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        let MeterRef { uuid, id: _ } = meter;
        let ledger = self.ledger()?;
        let mut items: Vec<StoredUsageRecord> = Vec::new();
        for entry in ledger.records() {
            if select(entry, *uuid, time_range, query, metadata_filter)
                .map_err(Untranslatable::into_plugin_error)?
            {
                items.push(entry.clone());
            }
        }

        // The sort, the seek and the page/keyset mint are the three steps
        // `contract_mutants::MutantLedger` mirrors verbatim, so they live
        // beside this module's other shared order helpers rather than in
        // two copies — see the comment above `order_is_ascending` for the
        // doubled correctness fix that duplication already cost.
        let ascending = order_is_ascending(query);
        sort_by_dispatched_order(&mut items, query, ascending);
        let items = match keyset {
            Some(ks) => seek_past_boundary(items, query, ks, ascending)?,
            None => items,
        };
        page_with_continuation(items, query, ascending)
    }

    /// A page from the ledger's own append order.
    ///
    /// Every entry is stamped with a monotonic sequence when it is admitted,
    /// and a position is the sequence of the last entry the read scanned,
    /// encoded as eight big-endian bytes so its bytewise ordering matches its
    /// numeric one. A page therefore **seeks** to a position rather than
    /// skipping to it: it resumes at the first entry whose sequence is greater
    /// than the one it was handed, never by walking a count of entries past
    /// the start — offset/limit scans are forbidden on both paginated paths,
    /// so a porter copying this into SQL carries over a key comparison rather
    /// than an `OFFSET n`.
    ///
    /// The compiled `scope` gates what the page **carries**, not what the
    /// position **counts**: an entry outside it is absent from `entries` while
    /// still advancing the cursor past itself. That split fixes a position's
    /// meaning independently of the scope and subscription it was issued under
    /// — it denotes the same prefix of the ledger under every grant — so any
    /// position resumes correctly for a caller whose grant differs, delivering
    /// every later entry that grant admits and skipping none. An
    /// untranslatable grant admits nothing, as on the point lookup.
    ///
    /// **Two grants are not in general handed the same position.** `limit`
    /// bounds the entries a page *admits* while the scan is unbounded, so a
    /// page that stops on the limit stops where its own grant's entries run
    /// out rather than where the ledger does: over a ledger alternating two
    /// tenants, `limit = 1` from `Oldest` hands one grant position 1 and the
    /// other position 2. Resumability is the property; two positions coincide
    /// only where both reads exhausted the ledger.
    ///
    /// **A cursor whose continuation retention has truncated is refused.**
    /// `FeedStart::After(position)` answers
    /// [`UsageCollectorPluginError::CursorBeyondRetention`] when some
    /// subscribed type's retention mark is greater than `position`. Neither
    /// the caller's scope nor the position's age is an input: the mark records
    /// what a sweep removed rather than what a grant would have delivered, so
    /// a cursor can be refused for an entry the caller's scope excluded, and a
    /// cursor older than the floor is served whole wherever this backend was
    /// never driven to drop past it. `FeedStart::Oldest` never consults a mark
    /// at all — it begins at the oldest entry still retained.
    ///
    /// # Errors
    ///
    /// [`UsageCollectorPluginError::CursorBeyondRetention`] for a `start`
    /// cursor whose continuation a drop has truncated, which is the one
    /// caller-actionable refusal on this path.
    ///
    /// [`UsageCollectorPluginError::Internal`] for a zero `limit` and for a
    /// position this backend did not issue. Both are host-contract breaches
    /// rather than caller faults: the published page limit is at least one,
    /// and a plugin owns its own position encoding.
    async fn read_feed_page(
        &self,
        subscription: &[MeterRef],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
        // A zero limit emits nothing and so advances the cursor past nothing,
        // making `next` a fixpoint: under an `until` the caller follows the
        // same page forever. The published limit is at least one, so a zero
        // one is a host-contract breach.
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

        // `FeedStart::Oldest` is exempt by construction rather than by a mark
        // that happens not to fire: it begins at the oldest entry still
        // retained, so nothing it asks for is missing.
        if matches!(start, FeedStart::After(_)) && ledger.retention_has_passed(subscription, from) {
            return Err(UsageCollectorPluginError::CursorBeyondRetention);
        }

        let mut entries = Vec::new();
        let mut cursor = from;
        // The seek: `sequence > from` resumes at the first entry the position
        // does not already cover, which is what makes the resumption a key
        // comparison rather than a count of rows to walk past.
        for entry in ledger.entries.iter().filter(|entry| entry.sequence > from) {
            // `upper` is a position, so it names an entry a bounded replay is
            // still asked to read; the replay stops at the first entry beyond
            // the one it names, which is `sequence > upper`.
            if entry.sequence > upper || u64::try_from(entries.len()).unwrap_or(u64::MAX) >= limit {
                break;
            }
            // `cursor` moves onto this entry's sequence whether or not the
            // subscription and scope admit it: advancing only past admitted
            // entries would make the position depend on who asked.
            cursor = entry.sequence;
            if subscription
                .iter()
                .any(|MeterRef { uuid, id: _ }| *uuid == entry.record.gts_type_uuid)
                && expr_admits(&entry.record, scope).unwrap_or(false)
            {
                entries.push(entry.record.clone());
            }
        }

        let next = if until.is_some() && cursor >= upper {
            None
        } else {
            Some(Self::encode_position(cursor)?)
        };
        Ok(FeedPage { entries, next })
    }

    /// Counters, the declared fold, and watermarks over the ledger.
    ///
    /// The compiled `scope` applies **before** the tenant and type arguments,
    /// so a tenant it excludes answers exactly as one holding no entries
    /// rather than as an error. An untranslatable grant admits nothing, the
    /// same disposition as the point lookup's.
    ///
    /// **`accepted_count` and the quantity summary disagree on purpose.** The
    /// count reports ingestion activity, so it keeps invalidations; the
    /// summary excludes withdrawn pairs. The summary's shape follows
    /// [`QuantitySummary::accrues`] over the `fold` parameter rather than this
    /// method's own decision. The withdrawn set is collected from the whole
    /// ledger rather than from the range or the scope, because an invalidation
    /// either leaves out has still been accepted and still withdraws its
    /// target.
    ///
    /// **Do not copy the repeated passes.** `in_scope` re-walks the ledger
    /// once per figure, which is free enough here under one held lock. A real
    /// backend does not need that many round trips — but it does need the
    /// selections kept apart, and folding them into one aggregate is the trap.
    /// The count and the summary are bounded by the requested range and differ
    /// only by the withdrawal exclusion, so they compose as `FILTER` clauses
    /// over one ranged scan. **The watermarks are not bounded by the range**,
    /// so an aggregate covering them too forces an unranged `FROM` — a full
    /// scan of the scope's history, where each watermark could otherwise be
    /// the leading edge of its own index (`usage_records_watermark_idx` and
    /// `usage_records_tenant_type_window_idx` in the `TimescaleDB` plugin).
    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        let MeterRef { uuid, id: _ } = meter;
        let ledger = self.ledger()?;
        let withdrawn = withdrawn_targets(&ledger);
        let in_scope = || {
            ledger
                .records()
                .filter(|e| expr_admits(e, scope).unwrap_or(false))
                .filter(|e| e.tenant_id == tenant_id && e.gts_type_uuid == *uuid)
        };

        // [`TimeRange::contains_window_end`] rather than an inlined boundary:
        // re-spelling it would be a second site to move.
        let accepted_count = in_scope()
            .filter(|e| time_range.contains_window_end(e.window_end))
            .count();

        // The fold's own selection: the same range, then both halves of every
        // withdrawn pair removed. The count above keeps them.
        let folded: Vec<&StoredUsageRecord> = in_scope()
            .filter(|e| time_range.contains_window_end(e.window_end))
            .filter(|e| e.invalidation.is_none() && !withdrawn.contains(&e.id))
            .collect();

        // Every field is given, so no `..ReconciliationMetadata::empty_for(fold)`
        // tail: `clippy::needless_update` fires on a struct update that updates
        // nothing.
        Ok(ReconciliationMetadata {
            accepted_count: u64::try_from(accepted_count).unwrap_or(u64::MAX),
            quantity_summary: if QuantitySummary::accrues(fold) {
                QuantitySummary::Accrued(accrued_sum(&folded)?)
            } else {
                QuantitySummary::Observations(latest_observation(&folded).map(|latest| {
                    ObservedQuantity {
                        count: NonZeroU64::new(u64::try_from(folded.len()).unwrap_or(u64::MAX))
                            .expect(
                                "`latest_observation` returns `Some` only when `folded` is \
                                 non-empty",
                            ),
                        latest,
                    }
                }))
            },
            max_accepted_at: in_scope().map(|e| e.accepted_at).max(),
            max_window_end: in_scope().map(|e| e.window_end).max(),
        })
    }
}

#[async_trait]
impl ContractRetention for InMemoryReferencePlugin {
    /// Runs this backend's retention over one meter, to one floor.
    ///
    /// Removes every entry of `meter` whose covered period ends before
    /// `floor` and raises that meter's mark to the highest sequence removed,
    /// which is what a later `read_feed_page` refuses a cursor on. Both
    /// happen under one lock acquisition, as a real sweep drops a chunk and
    /// raises its marks in one transaction.
    ///
    /// A meter this backend has never written, and a floor no entry falls
    /// before, both leave the ledger and the marks as they were — the trait
    /// asks for the state after the sweep, and that state is already it.
    ///
    /// # Errors
    ///
    /// A message naming the poisoned ledger lock, the one way this can fail.
    /// It is the harness's fault rather than the plugin's, which is why the
    /// trait reports a `String` rather than a [`UsageCollectorPluginError`].
    async fn drop_before(
        &self,
        meter: &MeterRef,
        floor: time::OffsetDateTime,
    ) -> Result<(), String> {
        let MeterRef { uuid, id: _ } = meter;
        let mut ledger = self
            .ledger()
            .map_err(|err| format!("the reference backend could not run retention: {err}"))?;
        ledger.drop_before(*uuid, floor);
        Ok(())
    }
}

// Admission

/// Decides one entry against the ledger it is being admitted to.
///
/// A collision on `id` already means the dedup-identity inputs agree (see
/// [`crate::id::derive_usage_record_id`]), so it is decided by the rest of
/// what the caller supplied ([`StoredUsageRecord::caller_supplied_eq`]):
/// equal is an idempotent replay answering with the stored entry, different is
/// [`UsageCollectorPluginError::IdempotencyConflict`] carrying it. A second
/// invalidation of one record takes this same branch, since every invalidation
/// repeats its target's idempotency key under `entry_type = invalidation`.
fn admit(
    ledger: &mut Ledger,
    record: StoredUsageRecord,
) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
    if let Some(stored) = ledger.records().find(|entry| entry.id == record.id) {
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

/// Every `StoredUsageRecord.id` an accepted invalidation names.
fn withdrawn_targets(ledger: &Ledger) -> std::collections::BTreeSet<Uuid> {
    ledger
        .records()
        .filter_map(|entry| entry.invalidation.as_ref().map(|inv| inv.target))
        .collect()
}

// Selection

/// A filter node this backend cannot translate at all.
///
/// **Distinct from "the row did not satisfy the filter", which is the whole
/// point of this type.** Folding the two into one `false` answers an
/// untranslatable query with an empty page — a silently wrong result rather
/// than a refusal. Which disposition is right depends on whose expression it
/// is:
///
/// * A **compiled PDP scope** that cannot be translated MUST exclude the row;
///   a scope this backend cannot interpret admitting anything is a
///   cross-tenant read.
/// * A **caller filter** that cannot be translated MUST refuse the query;
///   excluding silently answers a question the caller did not ask.
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
/// Period selection is [`TimeRange::contains_window_end`].
///
/// The filter is evaluated before the cheap predicates so an untranslatable
/// node is reported for any entry in the ledger rather than only for one
/// already inside the meter and the range. It is still a per-entry walk, so an
/// **empty** ledger reports nothing — sound, since with no entries the answer
/// is the empty page whatever the filter says.
fn select(
    entry: &StoredUsageRecord,
    gts_type_uuid: Uuid,
    time_range: TimeRange,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> Result<bool, Untranslatable> {
    let filter_admits = match query.filter() {
        Some(filter) => expr_admits(entry, filter)?,
        None => true,
    };
    Ok(filter_admits
        && entry.gts_type_uuid == gts_type_uuid
        && time_range.contains_window_end(entry.window_end)
        && metadata_admits(entry, metadata_filter))
}

/// Renders one record attribute as a *wire* raw-page sort key, in the
/// admissible `$orderby` vocabulary ([`crate::models::KEYSET_SAFE_RECORD_FIELDS`]).
///
/// **This exists to populate a [`Keyset`]'s returned values, and for nothing
/// else** — see [`OrderFieldValue`] for why comparing this rendering is
/// unsound, and [`order_field_value`] for what sorting and seeking compare.
///
/// **It has to agree with the `TimescaleDB` plugin's `record_row_key`**,
/// because a [`Keyset`]'s values are compared across backends by the same
/// checks: UUID fields render through [`Uuid`]'s `Display`, instants render
/// RFC 3339, and the remaining string fields render verbatim.
/// `the_reference_sort_key_matches_the_plugins_rendering_per_admissible_key`
/// pins that per field. An unknown `field` renders as an empty string rather
/// than panicking, matching [`order_field_value`]'s own degradation.
fn reference_sort_key(record: &StoredUsageRecord, field: &str) -> String {
    match field {
        "id" => record.id.to_string(),
        "window_start" => record.window_start.format(&Rfc3339).unwrap_or_default(),
        "window_end" => record.window_end.format(&Rfc3339).unwrap_or_default(),
        "tenant_id" => record.tenant_id.to_string(),
        "resource_id" => record.resource_ref.resource_id().to_owned(),
        "resource_type" => record.resource_ref.resource_type().to_owned(),
        "origin" => record.origin.as_str().to_owned(),
        "accepted_at" => record.accepted_at.format(&Rfc3339).unwrap_or_default(),
        _ => String::new(),
    }
}

/// One admissible-field value, typed for comparison — what `list_usage_records`
/// actually sorts and seeks by, as distinct from [`reference_sort_key`]'s
/// `String` rendering of the same admissible fields.
///
/// **Why two representations are needed.** Comparing two *renderings* of an
/// RFC 3339 instant is not comparing two *instants*: `time`'s `Rfc3339`
/// formatter omits a zero sub-second part and trims trailing zeros from a
/// non-zero one, so `"…:56Z"` and `"…:56.5Z"` differ first at `'Z'` (0x5A)
/// versus `'.'` (0x2E) — and `'.' < 'Z'` ranks the **later** instant first. A
/// real plugin binds a keyset value back to a typed `timestamptz` (or `uuid`)
/// and lets the database compare it, so this backend's sort and seek compare
/// [`OrderFieldValue`], never [`reference_sort_key`]'s `String`.
///
/// Deriving `Ord` is sound *because* every comparison is between two values
/// produced for the **same** `field` name, so the variant matches on both
/// sides and the derived comparison reduces to the inner typed value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum OrderFieldValue {
    /// `id` or `tenant_id`.
    Uuid(Uuid),
    /// `window_start`, `window_end` or `accepted_at`.
    Instant(time::OffsetDateTime),
    /// `resource_id`, `resource_type` or `origin`.
    Str(String),
}

/// The typed value `record` carries for one admissible order field — what
/// `list_usage_records`'s sort and seek compare. See [`OrderFieldValue`] for
/// why this exists beside [`reference_sort_key`]. An unknown `field` renders
/// as an empty string, matching that function's own fallback.
fn order_field_value(record: &StoredUsageRecord, field: &str) -> OrderFieldValue {
    match field {
        "id" => OrderFieldValue::Uuid(record.id),
        "tenant_id" => OrderFieldValue::Uuid(record.tenant_id),
        "window_start" => OrderFieldValue::Instant(record.window_start),
        "window_end" => OrderFieldValue::Instant(record.window_end),
        "resource_id" => OrderFieldValue::Str(record.resource_ref.resource_id().to_owned()),
        "resource_type" => OrderFieldValue::Str(record.resource_ref.resource_type().to_owned()),
        "origin" => OrderFieldValue::Str(record.origin.as_str().to_owned()),
        "accepted_at" => OrderFieldValue::Instant(record.accepted_at),
        _ => OrderFieldValue::Str(String::new()),
    }
}

/// Parses one keyset boundary value — [`reference_sort_key`]'s rendering,
/// handed back by the gateway inside a [`Keyset`] — into the same typed
/// form [`order_field_value`] produces, so a seek compares typed values on
/// both sides rather than a typed record value against a rendered string.
///
/// # Errors
///
/// [`UsageCollectorPluginError::Internal`] when `rendered` does not parse as
/// the type `field` names — a host-contract breach rather than a caller fault,
/// since the gateway never mutates a plugin's returned values.
fn parse_order_field_value(
    field: &str,
    rendered: &str,
) -> Result<OrderFieldValue, UsageCollectorPluginError> {
    match field {
        "id" | "tenant_id" => Uuid::parse_str(rendered)
            .map(OrderFieldValue::Uuid)
            .map_err(|err| {
                UsageCollectorPluginError::internal(format!(
                    "a keyset boundary for `{field}` is `{rendered}`, which does not parse as a \
                 UUID this backend could have rendered: {err}"
                ))
            }),
        "window_start" | "window_end" | "accepted_at" => {
            time::OffsetDateTime::parse(rendered, &Rfc3339)
                .map(OrderFieldValue::Instant)
                .map_err(|err| {
                    UsageCollectorPluginError::internal(format!(
                        "a keyset boundary for `{field}` is `{rendered}`, which does not parse as \
                     RFC 3339: {err}"
                    ))
                })
        }
        _ => Ok(OrderFieldValue::Str(rendered.to_owned())),
    }
}

// ── The raw-page walk, shared with the mirror ────────────────────────────────
//
// `contract_mutants::MutantLedger::list_usage_records` has to behave like
// [`InMemoryReferencePlugin::list_usage_records`] in every respect its one
// defect is not about, so it shares the steps below rather than carrying a
// copy of them.
//
// Split into separate steps rather than one function, because the mirror
// substitutes exactly one: [`Defect::SkipsRowsRatherThanSeeksTheBoundaryKey`]
// replaces the seek with a count-based skip, and a single
// `walk(items, query, keyset)` would need a defect parameter to express that
// — putting a switch for deliberate wrongness inside the exemplar.
//
// These are the module's only `pub(super)` items, which is what lets
// `reference_sort_key`, `order_field_value` and `parse_order_field_value`
// stay private: the mirror reaches them through these and so cannot
// hand-write a second comparison.
//
// Each takes the already-selected rows, because selection is the one part the
// two genuinely differ on.

/// Whether the dispatched order sorts ascending.
///
/// Read off the **first** key alone, which is sound only because the
/// gateway guarantees `query.order` is uniform in direction; an empty order
/// — which the gateway also guarantees cannot arrive — reads as ascending
/// so an in-process caller still gets a well-formed answer.
pub(super) fn order_is_ascending(query: &ODataQuery) -> bool {
    query
        .order
        .0
        .first()
        .is_none_or(|key| matches!(key.dir, SortDir::Asc))
}

/// Sorts `items` into the dispatched order.
///
/// Honours the dispatched order rather than the canonical pair alone. Every
/// key is a never-null record attribute
/// ([`crate::models::is_keyset_safe_record_field`] is the gateway's allowlist)
/// and the order is uniform in direction, so a stable sort over the keys in
/// sequence reproduces the tuple a SQL `ORDER BY` would. Compared as typed
/// [`OrderFieldValue`]s, never as [`reference_sort_key`]'s rendered strings.
pub(super) fn sort_by_dispatched_order(
    items: &mut [StoredUsageRecord],
    query: &ODataQuery,
    ascending: bool,
) {
    items.sort_by(|left, right| {
        let ord = query
            .order
            .0
            .iter()
            .map(|key| {
                order_field_value(left, &key.field).cmp(&order_field_value(right, &key.field))
            })
            .find(|ord| !ord.is_eq())
            .unwrap_or(core::cmp::Ordering::Equal);
        if ascending { ord } else { ord.reverse() }
    });
}

/// Drops every already-sorted row at or before the continuation boundary.
///
/// Seek, never skip. `keyset` carries the previous page's last row's values
/// under this same order, so the resumption is a key comparison and not a
/// count of entries walked past. The boundary is parsed back to
/// [`OrderFieldValue`] once, up front, so the per-row comparison inside the
/// loop is typed-to-typed — [`reference_sort_key`]'s rendering is a wire
/// format for the returned [`Keyset`], never a comparable one.
///
/// `.zip` truncates to the shorter of `query.order` and `keyset.values()`
/// rather than comparing lengths, deliberately: a keyset of the wrong arity
/// for the dispatched order is a host-contract breach the gateway guarantees
/// cannot reach here, so this is a precondition the walk assumes rather than
/// caller input it mishandles.
///
/// # Errors
///
/// [`UsageCollectorPluginError::Internal`] when a boundary value does not
/// parse back as the type its field names — see
/// [`parse_order_field_value`].
pub(super) fn seek_past_boundary(
    items: Vec<StoredUsageRecord>,
    query: &ODataQuery,
    keyset: &Keyset,
    ascending: bool,
) -> Result<Vec<StoredUsageRecord>, UsageCollectorPluginError> {
    let boundary: Vec<OrderFieldValue> = query
        .order
        .0
        .iter()
        .zip(keyset.values())
        .map(|(key, rendered)| parse_order_field_value(&key.field, rendered))
        .collect::<Result<_, _>>()?;
    Ok(items
        .into_iter()
        .skip_while(|record| {
            let ord = query
                .order
                .0
                .iter()
                .zip(&boundary)
                .map(|(key, boundary_value)| {
                    order_field_value(record, &key.field).cmp(boundary_value)
                })
                .find(|ord| !ord.is_eq())
                .unwrap_or(core::cmp::Ordering::Equal);
            if ascending {
                ord != core::cmp::Ordering::Greater
            } else {
                ord != core::cmp::Ordering::Less
            }
        })
        .collect())
}

/// Trims the walked rows to the caller's limit and mints the continuation.
///
/// The look-ahead is one row past the caller's limit, trimmed before the page
/// is returned and used only to decide whether `next` is `Some`. With no
/// caller limit the whole selection is served as one page: the gear always
/// populates `query.limit` before dispatch, so the `u64::MAX` fallback exists
/// only so an in-process caller still gets a well-formed answer.
///
/// # Errors
///
/// [`UsageCollectorPluginError::Internal`] when a page reporting a
/// continuation has no last row to mint it from, or when the minted values
/// do not form an admissible [`Keyset`].
pub(super) fn page_with_continuation(
    items: Vec<StoredUsageRecord>,
    query: &ODataQuery,
    ascending: bool,
) -> Result<RecordPage, UsageCollectorPluginError> {
    let page_size = usize::try_from(query.limit.unwrap_or(u64::MAX)).unwrap_or(usize::MAX);
    let has_next = items.len() > page_size;
    let items: Vec<StoredUsageRecord> = items.into_iter().take(page_size).collect();
    let next = if has_next {
        let last = items
            .last()
            .ok_or_else(|| UsageCollectorPluginError::internal("non-empty page lost its tail"))?;
        let values: Vec<String> = query
            .order
            .0
            .iter()
            .map(|key| reference_sort_key(last, &key.field))
            .collect();
        Some(
            Keyset::new(
                values,
                if ascending {
                    SortDir::Asc
                } else {
                    SortDir::Desc
                },
            )
            .map_err(|err| UsageCollectorPluginError::internal(err.to_string()))?,
        )
    } else {
        None
    };
    Ok(RecordPage { items, next })
}

/// Whether an entry satisfies every metadata filter in the slice.
///
/// AND across the slice, including two entries naming the same key; OR
/// within one entry's values; an empty slice imposes nothing. Merging
/// same-key entries would widen the result set, which is why this iterates
/// them rather than grouping them.
fn metadata_admits(entry: &StoredUsageRecord, filters: &[MetadataFilter]) -> bool {
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
/// The expression is either the gear's compiled PDP scope or that scope
/// AND-ed with the caller's `$filter`, so this evaluator serves both.
///
/// **`Ok(false)` and `Err` are different answers.** `Ok(false)` is "this row
/// is not selected"; `Err` is "this expression cannot be translated", whose
/// dispositions are on [`Untranslatable`]. A plugin projecting to SQL owes the
/// same split — an untranslatable predicate is a refused query, never a
/// dropped conjunct, and an untranslatable scope grants nothing.
///
/// [`ast::Expr::Not`] is deliberately untranslatable: negating a subtree that
/// answered `false` only because it could not be read would turn a fail-closed
/// answer into `true`. Nothing in the gear emits one — `scope_to_odata_filter`
/// builds `And`, `Or`, `Compare(_, Eq, _)` and `In` and nothing else.
///
/// `Compare(_, Ne, _)` is the other negation context and *is* translated,
/// since `query.filter` is caller-supplied rather than restricted to what the
/// gear compiles. That is safe only because [`value_matches`] reports "cannot
/// compare" as an outcome of its own; see [`compare_admits`].
///
/// Both operands of `And` and `Or` are evaluated rather than short-circuited,
/// so translatability does not depend on which row the expression met first.
fn expr_admits(entry: &StoredUsageRecord, expr: &ast::Expr) -> Result<bool, Untranslatable> {
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
/// **`ne` is a negation context,** so everything that stops a comparison has
/// to say whether it stopped because the row did not match or because the
/// comparison could not be made:
///
/// * **A known attribute this row does not carry** — [`FieldLookup::Null`],
///   and `Ok(false)` under both operators, matching SQL three-valued logic.
///   So `subject_id ne 'x'` on a subject-less row is `Ok(false)`.
/// * **An identifier outside the vocabulary** — [`FieldLookup::Unknown`],
///   untranslatable.
/// * **A pairing that cannot be compared** — [`value_matches`] answering
///   `None`, untranslatable.
///
/// Ordering operators and any operand shape other than
/// `<identifier> <op> <literal>` are untranslatable too.
fn compare_admits(
    entry: &StoredUsageRecord,
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
            // `matched` under `eq`, `!matched` under `ne` — reached only for a
            // comparison that was actually made.
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
    entry: &StoredUsageRecord,
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
/// The vocabulary is `UsageRecordQuery`'s, minus the covered-period bounds:
/// those are reserved on the `$filter` surface (the range travels as a typed
/// parameter), so they are [`FieldLookup::Unknown`] here, which refuses a
/// caller filter naming one instead of answering it.
fn record_field<'a>(entry: &'a StoredUsageRecord, name: &str) -> FieldLookup<'a> {
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
/// [`ast::Value`]'s own `Display` renders exactly this, and this function
/// exists anyway so [`value_matches`] never reaches for `Display` because it
/// was convenient.
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
/// **Never route this through [`ast::Value`]'s `Display`.** That impl prints
/// the *type name* — `"uuid"`, `"string"` — not the value, so a comparison
/// built on it would find every UUID equal to every other. `tenant_id`
/// compiles to [`ast::Value::Uuid`] while the other attribution identifiers
/// compile to [`ast::Value::String`], so both spellings are matched here.
///
/// The cross-typed pairs mirror the gear's own coercion policy
/// (`authz::coerce_scope_value`): a UUID-typed field accepts a UUID-shaped
/// string and vice versa, so a `resource_id` that happens to be a UUID matches
/// however the PEP compiler typed it.
///
/// **`None` means the pairing cannot be compared, distinct from
/// `Some(false)`.** Collapsing the two reads correctly under `eq` and inverts
/// under `ne`, where an uncomparable pairing would come back `true` and admit
/// the row. The gear denies exactly that mismatch, so anything but "cannot
/// compare" would leave this backend more permissive than the gear whose
/// scopes it models.
fn value_matches(field: FieldValue<'_>, value: &ast::Value) -> Option<bool> {
    match (field, value) {
        (FieldValue::Uuid(left), ast::Value::Uuid(right)) => Some(left == *right),
        // `Some(false)` when the string is a UUID that differs, `None` when it
        // is not a UUID at all.
        (FieldValue::Uuid(left), ast::Value::String(right)) => {
            Uuid::parse_str(right).ok().map(|right| left == right)
        }
        (FieldValue::Str(left), ast::Value::String(right)) => Some(left == right),
        (FieldValue::Str(left), ast::Value::Uuid(right)) => Some(left == right.to_string()),
        // Every other literal kind: no comparison this backend can make.
        _ => None,
    }
}

// Aggregation

/// Groups the selected rows and folds each group.
///
/// An empty `group_by` is the no-grouping case: exactly one bucket carrying an
/// empty key, emitted even over an empty selection. What that bucket carries
/// is [`fold_value`]'s to decide, and it splits by fold.
///
/// **A grouping empties differently, and deliberately so.** The groups below
/// are keyed from the surviving rows alone, so a group nothing survives in is
/// never keyed and no bucket is emitted for it — the emptiness lands in the
/// grouping rather than in a fold over an empty group, which is why the split
/// above is unreachable through this branch.
///
/// The bucket count is capped at [`MAX_AGGREGATION_BUCKETS`] `+ 1`. The SDK
/// puts that bound on the plugin rather than the gateway so an unbounded
/// grouping cannot materialise in plugin memory; the extra bucket is what lets
/// the gateway tell "at the cap" from "over it".
fn fold_rows(
    fold: AggregationFold,
    rows: &[&StoredUsageRecord],
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

    let mut groups: BTreeMap<Vec<String>, Vec<&StoredUsageRecord>> = BTreeMap::new();
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
/// A row with no subject is excluded from a `subject_id` grouping rather than
/// bucketed under an empty string, and the same holds for `subject_type` and
/// for a metadata key the row does not carry — see the module docs' "Stated
/// limits".
fn bucket_key(row: &StoredUsageRecord, group_by: &[AggregationDimension]) -> Option<Vec<String>> {
    group_by
        .iter()
        .map(|dimension| match dimension {
            // `Uuid::to_string()` — lowercase, hyphenated — is the fixed key
            // encoding for this dimension.
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
/// **An empty bucket splits by fold.** `SUM` and `COUNT` are defined over an
/// empty selection and report `0`; `MAX`, `MIN` and `LATEST` are not and
/// report absent.
///
/// **`LATEST` takes the greatest `(window_end, accepted_at, id)`**, the
/// declared order. The `id` key is a byte comparison by construction: [`Uuid`]
/// is a `#[repr(transparent)]` newtype over `[u8; 16]` with a derived `Ord`.
/// It is also what makes the order total, so the answer never depends on
/// ledger insertion order. Every key compares across tenants and types, so the
/// rule holds unchanged for a group spanning either.
fn fold_value(
    fold: AggregationFold,
    rows: &[&StoredUsageRecord],
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
            return Ok(Some(total));
        }
        AggregationFold::Max => rows.iter().max_by_key(|row| row.quantity.as_decimal()),
        AggregationFold::Min => rows.iter().min_by_key(|row| row.quantity.as_decimal()),
        AggregationFold::Latest => rows
            .iter()
            .max_by_key(|row| (row.window_end, row.accepted_at, row.id)),
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

/// The accrued total over the selection, for the `SUM` branch of
/// [`QuantitySummary`].
///
/// Separate from [`fold_value`] rather than a call into it, because the
/// reconciliation summary is **not** the declared fold's value: it is one of
/// two fixed shapes, and only the `SUM` shape coincides with a fold.
fn accrued_sum(rows: &[&StoredUsageRecord]) -> Result<BigDecimal, UsageCollectorPluginError> {
    let mut total = BigDecimal::from(0);
    for row in rows {
        total += to_big_decimal(row.quantity.as_decimal())?;
    }
    Ok(total)
}

/// The latest observation by the `LATEST` total order — greatest
/// `window_end`, then `accepted_at`, then `id` — the same order
/// [`fold_value`]'s `Latest` arm uses. Reported for **every** non-accruing
/// fold, `MAX` and `MIN` included: those select the summary's *branch*, never
/// its ordering.
fn latest_observation(rows: &[&StoredUsageRecord]) -> Option<UsageQuantity> {
    rows.iter()
        .max_by_key(|row| (row.window_end, row.accepted_at, row.id))
        .map(|row| row.quantity)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod reference_sort_key_tests {
    use super::{Rfc3339, reference_sort_key};
    use crate::contract::fixtures::fixture_record_for_tenant;
    use crate::models::{IdempotencyKey, KEYSET_SAFE_RECORD_FIELDS, RecordOrigin};
    use crate::quantity::UsageQuantity;

    /// [`reference_sort_key`]'s rendering of every admissible keyset field
    /// has to agree with the `TimescaleDB` plugin's `record_row_key`, because
    /// a [`crate::keyset::Keyset`]'s values are compared across backends by
    /// the very same checks; a disagreement makes a check pass on one backend
    /// and fail on the other for a reason unrelated to conformance.
    ///
    /// It **iterates [`KEYSET_SAFE_RECORD_FIELDS`] rather than listing the
    /// names**, so a field later added to that list fails the `panic!` arm
    /// below rather than leaving this test green while the widest admissible
    /// keyset silently grew a field it never looked at.
    #[test]
    fn the_reference_sort_key_matches_the_plugins_rendering_per_admissible_key() {
        let idempotency_key = IdempotencyKey::new("reference-sort-key-agreement")
            .expect("the test's own idempotency key is valid");
        let window_start = rfc3339_literal("2021-06-15T12:34:56.123456Z");
        let window_end = rfc3339_literal("2021-06-15T13:00:00.654321Z");
        let mut record = fixture_record_for_tenant(
            uuid::Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_c0ff_ee01),
            &idempotency_key,
            UsageQuantity::parse("1").expect("1 is inside the published range"),
            window_start,
            window_end,
        )
        .expect("the test's own fixture is well formed");
        // Distinguishes `origin` from the fixture builder's default.
        record.origin = RecordOrigin::Backfill;
        // Distinguishes `accepted_at` from the builder's default, which is a
        // round day boundary with no fractional second — that would make the
        // expectation agree by construction rather than against a hand-written
        // literal pinning the RFC 3339 format itself, which is also why the
        // period bounds below are literals.
        record.accepted_at = rfc3339_literal("2022-03-10T08:15:30.987654Z");

        for field in KEYSET_SAFE_RECORD_FIELDS {
            let observed = reference_sort_key(&record, field);
            let expected = match *field {
                "id" => record.id.to_string(),
                "window_start" => "2021-06-15T12:34:56.123456Z".to_owned(),
                "window_end" => "2021-06-15T13:00:00.654321Z".to_owned(),
                "tenant_id" => record.tenant_id.to_string(),
                "resource_id" => record.resource_ref.resource_id().to_owned(),
                "resource_type" => record.resource_ref.resource_type().to_owned(),
                "origin" => "backfill".to_owned(),
                "accepted_at" => "2022-03-10T08:15:30.987654Z".to_owned(),
                other => panic!(
                    "KEYSET_SAFE_RECORD_FIELDS grew a field (`{other}`) this test does not yet \
                     hand-render; add an expectation for it above rather than leaving this test \
                     silently short of the widest admissible keyset"
                ),
            };
            assert_eq!(
                observed, expected,
                "reference_sort_key(\"{field}\") rendered `{observed}`, and the plugin's \
                 `record_row_key` renders the equivalent column as `{expected}`. A keyset's \
                 values are compared across backends by the same checks, so the two renderings \
                 have to agree digit for digit and character for character."
            );
        }
    }

    /// A microsecond-precision RFC 3339 literal, parsed rather than
    /// hand-assembled: `time::macros::datetime!` is unavailable here, and
    /// parsing the exact literal this test compares against keeps the fixture
    /// and the expectation from drifting apart.
    fn rfc3339_literal(literal: &str) -> time::OffsetDateTime {
        time::OffsetDateTime::parse(literal, &Rfc3339).unwrap_or_else(|err| {
            panic!("the test's own literal `{literal}` is not RFC 3339: {err}")
        })
    }

    /// `list_usage_records`'s sort and seek must compare typed instants, never
    /// [`reference_sort_key`]'s RFC 3339 *rendering* of them: the two diverge
    /// whenever instants differ only in sub-second precision, because `time`'s
    /// `Rfc3339` formatter omits a zero sub-second part, so `"…T00:00:00Z"`
    /// and `"…T00:00:00.5Z"` first differ at `'Z'` against `'.'` and
    /// `'.' < 'Z'` ranks the chronologically **later** instant first.
    ///
    /// Three entries, chronological `A < B < C`, where only `B`'s `window_end`
    /// carries a fraction — the shape that inverts under a rendering
    /// comparison. Reading them back under `window_end` ascending at a page
    /// limit of one, so the walk exercises the seek as well as the sort, must
    /// deliver `A, B, C`; a rendering-based implementation delivers `B, A, C`.
    #[tokio::test]
    async fn the_sort_and_seek_compare_chronological_instants_not_rendered_strings() {
        use crate::contract::fixtures::{contract_meter, fixture_record_for_tenant};
        use crate::keyset::Keyset;
        use crate::plugin_api::UsageCollectorPluginV1;
        use crate::time_range::TimeRange;
        use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};

        let plugin = super::InMemoryReferencePlugin::new();
        let tenant_id = uuid::Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0ad0_0001);
        // It has to be the suite's shared meter: `fixture_record_for_tenant`
        // stamps every entry with `contract_meter()`'s reference and this
        // backend selects on the reference, so a hand-picked one would read
        // back an empty page whatever the sort did.
        let meter = contract_meter().expect("the shared contract meter is valid");

        let a_end = rfc3339_literal("2024-03-01T00:00:00Z");
        let b_end = rfc3339_literal("2024-03-01T00:00:00.5Z");
        let c_end = rfc3339_literal("2024-03-01T00:00:01Z");

        let mut ids: Vec<(&str, uuid::Uuid)> = Vec::new();
        for (label, window_end) in [("a", a_end), ("b", b_end), ("c", c_end)] {
            let idempotency_key = IdempotencyKey::new(format!("precision-regression-{label}"))
                .expect("a valid idempotency key");
            let record = fixture_record_for_tenant(
                tenant_id,
                &idempotency_key,
                UsageQuantity::parse("1").expect("1 is a valid quantity"),
                window_end.saturating_sub(time::Duration::hours(1)),
                window_end,
            )
            .expect("a well-formed fixture");
            ids.push((label, record.id));
            crate::contract::fixtures::seed_usage_record(&plugin, &meter, record)
                .await
                .expect("the fixture is accepted");
        }

        let range = TimeRange::new(
            a_end.saturating_sub(time::Duration::hours(1)),
            c_end.saturating_add(time::Duration::seconds(1)),
        )
        .expect("a valid range");
        let order = ODataOrderBy(vec![OrderKey {
            field: "window_end".to_owned(),
            dir: SortDir::Asc,
        }]);
        let label_of = |id: uuid::Uuid| {
            ids.iter()
                .find(|(_, candidate)| *candidate == id)
                .map_or("?", |(label, _)| label)
        };

        // The sort: one unpaginated read must deliver chronological order.
        let query = ODataQuery::new().with_order(order.clone()).with_limit(10);
        let page = plugin
            .list_usage_records(&meter, range, &query, &[], None)
            .await
            .expect("the read succeeds");
        let observed: Vec<&str> = page.items.iter().map(|item| label_of(item.id)).collect();
        assert_eq!(
            observed,
            vec!["a", "b", "c"],
            "window_end order must be chronological (A < B < C). A rendering comparison \
             delivers B before A, because the rendered strings first differ at 'Z' versus '.', \
             and '.' sorts before 'Z' even though A is chronologically first"
        );

        // The seek: a limit-one walk must resume correctly across the same
        // sub-second boundary, not just sort it once.
        let mut keyset: Option<Keyset> = None;
        let mut walked: Vec<&str> = Vec::new();
        for _ in 0..4 {
            let query = ODataQuery::new().with_order(order.clone()).with_limit(1);
            let page = plugin
                .list_usage_records(&meter, range, &query, &[], keyset.as_ref())
                .await
                .expect("the read succeeds");
            walked.extend(page.items.iter().map(|item| label_of(item.id)));
            match page.next {
                Some(next) => keyset = Some(next),
                None => break,
            }
        }
        assert_eq!(
            walked,
            vec!["a", "b", "c"],
            "a limit-one walk must seek past each sub-second boundary in chronological order, \
             not the lexical order of the rendered keyset values"
        );
    }
}
