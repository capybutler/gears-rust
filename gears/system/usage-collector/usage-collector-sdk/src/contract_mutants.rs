//! Deliberately non-conforming backends, one per rule the suite checks.
//!
//! The contract suite and [`InMemoryReferencePlugin`] were written alongside
//! each other, so a green `the_reference_backend_conforms` establishes that
//! the suite *runs*. It establishes nothing about whether any check would
//! notice a non-conforming plugin, and a check that cannot fail is worse
//! than a missing one — a port is accepted on it, and it reads as coverage.
//! The subjects here are what close that: each is **behaviourally** the
//! reference backend wrong in exactly one plausible way, and
//! `each_check_fails_against_its_own_defect_and_no_other` asserts a full
//! column against each of them.
//!
//! *Behaviourally* is the exact word. Three subjects wrap a real reference
//! backend and are that backend plus one interception; the other three
//! re-implement it, and a re-implementation is the same backend only as far
//! as the checks can see. [`MutantLedger`] states how far that is.
//!
//! # Why none of this lives in `reference`
//!
//! [`super::reference`] is the one worked implementation of this SPI that
//! exists, and a plugin author copies it when porting a real backend.
//! A defect switch there — a flag, a test hook, a `#[cfg(test)]` branch —
//! would put deliberate wrongness inside the exemplar. So the mutants are
//! test-only code that either wraps the reference or carries a ledger of
//! their own, and `reference.rs` is untouched.
//!
//! # Two shapes, and which defect gets which
//!
//! **A wrapper** ([`WrappedReference`]) delegates to a real
//! [`InMemoryReferencePlugin`] and intercepts one method. Everything the
//! defect is not about is then the exemplar's own behaviour, which is the
//! strongest form the subject can take. Three defects fit: one rewrites the
//! quantity on the way in, one keeps a dedup index beside the ledger, one
//! substitutes the scope on the point read.
//!
//! **A ledger of its own** ([`MutantLedger`]) is needed by the other three,
//! because each changes a predicate the inner backend owns and no
//! interception can reach it: which column a range meets, which rows a fold
//! walks, and whether a batch is decided under one lock. It mirrors the
//! reference where the defect is not, and it is smaller in two stated ways
//! that no check reaches — see [`MutantLedger`].

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo, ast};
use uuid::Uuid;

use super::reference::InMemoryReferencePlugin;
use crate::error::UsageCollectorPluginError;
use crate::models::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult, MetadataFilter,
    MeterTypeId, UsageRecord,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

/// A backend that is the reference backend with one rule broken.
///
/// One rule each, deliberately. A mutant wrong in two ways fails two checks
/// and proves neither of them was the one that noticed — the suite would
/// look discriminating while one of its checks did nothing.
///
/// Each defect is the *plausible* wrong implementation, not an absurd one. A
/// backend that returns garbage is caught by anything; the question this
/// test answers is whether the suite catches the mistake someone would
/// actually make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Defect {
    /// Stores the quantity through an `f64` — the mistake a backend makes by
    /// choosing a `double precision` column.
    QuantityThroughFloat,
    /// Selects on `window_start` instead of `window_end` — the mistake a
    /// backend makes by porting the pre-period point-in-time column.
    SelectsOnWindowStart,
    /// Keys dedup on `(tenant, type, idempotency_key)`, omitting the period
    /// bounds. The pre-period identity.
    DedupIgnoresThePeriod,
    /// Excludes the withdrawn record from the fold but folds the
    /// invalidation. DESIGN names this one: it double-counts the withdrawn
    /// measurement.
    FoldsTheInvalidation,
    /// Checks for an existing invalidation, then inserts, without holding
    /// the lock across both.
    ChecksThenInsertsTheInvalidation,
    /// Honours the scope on the list and aggregate paths and ignores it on
    /// the point read.
    IgnoresScopeOnThePointRead,
}

/// The subject one defect names, ready to be handed to
/// [`run_all`](super::run_all).
///
/// Erased behind a `Box<dyn …>` because the two shapes are different types
/// and the caller has no business knowing which one a defect took: the point
/// of the matrix is that every mutant is a backend the suite may be pointed
/// at, and a caller that could tell them apart could be tempted to expect
/// different things of them.
pub(super) fn mutant(defect: Defect) -> Box<dyn UsageCollectorPluginV1> {
    match defect {
        Defect::QuantityThroughFloat
        | Defect::DedupIgnoresThePeriod
        | Defect::IgnoresScopeOnThePointRead => Box::new(WrappedReference::new(defect)),
        Defect::SelectsOnWindowStart
        | Defect::FoldsTheInvalidation
        | Defect::ChecksThenInsertsTheInvalidation => Box::new(MutantLedger::new(defect)),
    }
}

// ---------------------------------------------------------------------------
// The wrapping shape
// ---------------------------------------------------------------------------

/// A real [`InMemoryReferencePlugin`] with one method intercepted.
///
/// Every path the defect is not about is the exemplar's own code, so a
/// failure against one of these subjects is a failure against a conforming
/// backend plus exactly the named mistake.
///
/// One qualification, and it holds for all three wrapped defects:
/// [`Self::create_usage_records`] is not pure delegation. The inner backend
/// still decides the batch, but the per-entry alignment around it — which
/// entries reach it, and where a refusal of this wrapper's own lands in the
/// answer — is code written here. See that method's doc.
struct WrappedReference {
    /// The conforming backend everything is delegated to.
    inner: InMemoryReferencePlugin,
    /// Which rule this subject breaks.
    defect: Defect,
    /// The period-blind dedup index [`Defect::DedupIgnoresThePeriod`] keys
    /// on: `(tenant_id, gts_type_id, idempotency_key)` to the entry that
    /// claimed it. Unused by the other two defects.
    ///
    /// A claim is recorded when the entry is admitted rather than after the
    /// inner backend stores it, which is a unique index written inside the
    /// same transaction. It therefore leaves a claim behind for an entry the
    /// inner backend then refuses, and `at-most-one-invalidation` submits
    /// **two** such entries: its sequential second withdrawal, and whichever
    /// of its two racing withdrawals the batch turns down. Both are
    /// unobservable here, because neither key is resubmitted over a second
    /// period — the only question this index is ever asked.
    period_blind_keys: Mutex<BTreeMap<PeriodBlindKey, Uuid>>,
}

/// The three attributes the pre-period dedup identity keyed on:
/// `(tenant_id, gts_type_id, idempotency_key)`.
///
/// Named for what it leaves out. The derived identity reads five attributes,
/// and the two missing here are the covered-period bounds — which is the
/// whole of [`Defect::DedupIgnoresThePeriod`].
type PeriodBlindKey = (Uuid, String, String);

impl WrappedReference {
    /// Wraps a fresh reference backend.
    fn new(defect: Defect) -> Self {
        Self {
            inner: InMemoryReferencePlugin::new(),
            defect,
            period_blind_keys: Mutex::new(BTreeMap::new()),
        }
    }

    /// The defect's effect on one entry on its way in: a rewritten quantity,
    /// a refusal from the period-blind index, or the entry untouched.
    ///
    /// Synchronous, and it holds the index lock only for its own body: the
    /// caller awaits the inner backend afterwards, never while holding it.
    fn on_admission(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError> {
        match self.defect {
            // A `double precision` column: the value is whatever survives
            // the trip through the binary float.
            Defect::QuantityThroughFloat => Ok(UsageRecord {
                value: through_f64(record.value),
                ..record
            }),
            Defect::DedupIgnoresThePeriod => self.claim_period_blind_key(record),
            // Enumerated rather than caught by a wildcard. A seventh wrapped
            // defect routed here and forgotten would otherwise pass its
            // entries through untouched and report no violation at all;
            // spelling the variants out makes that a compile error instead
            // of a matrix row whose subject does nothing. The point read's
            // defect is applied on the read path, and the last three never
            // reach this type at all — `mutant` routes them to
            // `MutantLedger` — but exhaustiveness is the whole point.
            Defect::IgnoresScopeOnThePointRead
            | Defect::SelectsOnWindowStart
            | Defect::FoldsTheInvalidation
            | Defect::ChecksThenInsertsTheInvalidation => Ok(record),
        }
    }

    /// Admits an entry only if no other entry already holds its
    /// `(tenant, type, idempotency_key)`.
    ///
    /// This is a unique index over the three attributes the pre-period model
    /// keyed on, and it is the whole of the defect: the two covered-period
    /// bounds are part of the derived identity and invisible to this index,
    /// so one key over two periods collides here and is one entry rather
    /// than two.
    ///
    /// A resubmission of an entry that already claimed the key carries the
    /// same derived `id`, so an idempotent replay still reaches the inner
    /// backend and is answered by it.
    fn claim_period_blind_key(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let key = (
            record.tenant_id,
            record.gts_type_id.as_str().to_owned(),
            record.idempotency_key.as_str().to_owned(),
        );
        let mut claimed = self.period_blind_keys.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's dedup index lock is poisoned")
        })?;
        if let Some(existing) = claimed.get(&key)
            && *existing != record.id
        {
            return Err(UsageCollectorPluginError::IdempotencyConflict {
                idempotency_key: record.idempotency_key.as_str().to_owned(),
                existing_id: *existing,
            });
        }
        claimed.insert(key, record.id);
        Ok(record)
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for WrappedReference {
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let record = self.on_admission(record)?;
        self.inner.create_usage_record(record).await
    }

    /// The batch, with the defect applied per entry and the survivors handed
    /// to the inner backend in **one** call.
    ///
    /// Passing the survivors as a batch rather than admitting them one at a
    /// time is what keeps the at-most-one race the inner backend's to
    /// decide. Admitting them singly would also answer it correctly here,
    /// and it would mean these three subjects passed
    /// `at-most-one-invalidation` for a reason of the wrapper's own rather
    /// than the exemplar's.
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
        let prepared: Vec<Result<UsageRecord, UsageCollectorPluginError>> = records
            .into_iter()
            .map(|record| self.on_admission(record))
            .collect();
        let survivors: Vec<UsageRecord> = prepared
            .iter()
            .filter_map(|entry| entry.as_ref().ok().cloned())
            .collect();
        // A batch every entry of which this wrapper refused must not reach
        // the inner backend: the reference answers an empty batch with
        // `Internal`, and this call would then fail outright rather than
        // reporting the per-entry refusals it already has. Unreachable
        // today — only `DedupIgnoresThePeriod` refuses anything here, and
        // the one batch the suite sends it carries two distinct keys — which
        // is also why the emptiness guard above is repeated rather than left
        // to the inner backend to raise.
        let inner = if survivors.is_empty() {
            Vec::new()
        } else {
            self.inner.create_usage_records(survivors).await?
        };
        let mut inner = inner.into_iter();
        Ok(prepared
            .into_iter()
            .map(|entry| match entry {
                Ok(_) => inner.next().unwrap_or_else(|| {
                    Err(UsageCollectorPluginError::internal(
                        "the inner backend answered fewer outcomes than the batch carried entries",
                    ))
                }),
                Err(err) => Err(err),
            })
            .collect())
    }

    /// The point read, under the dispatched scope or under a scope that
    /// names only the row asked for.
    ///
    /// `id eq <the id asked for>` is what `SELECT * FROM usage_records WHERE
    /// id = $1` compiles to: the scope argument is dropped and nothing else
    /// changes, which is the mistake as a backend makes it rather than a
    /// caricature of it.
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if self.defect == Defect::IgnoresScopeOnThePointRead {
            return self.inner.get_usage_record(id, &only_this_row(id)).await;
        }
        self.inner.get_usage_record(id, scope).await
    }

    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.inner
            .query_aggregated_usage_records(
                gts_type_id,
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
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        self.inner
            .list_usage_records(gts_type_id, time_range, query, metadata_filter)
            .await
    }
}

/// One round trip of a quantity through a binary float.
///
/// Three of the published range's five corners move, which is what a
/// `double precision` column costs and what `quantity-round-trip` exists to
/// find: both magnitude corners lose their low digits
/// (`9999999999999999999999999999` comes back
/// `9999999999999999583119736832`), and `42.500` comes back `42.5`, its
/// scale normalised away.
///
/// **The two `1e-28` corners survive, and a real float column would not lose
/// them either.** `to_f64` renders `1e-28` as `1.0000000000000001e-28` and
/// `from_f64` lands it back exactly, because `Decimal`'s 28-digit scale cap
/// truncates the float's excess digits onto the original value. Nothing is
/// being spared here — the smallest published value simply is not where a
/// binary float loses, so this subject is as wrong as the column it models
/// rather than kinder than it.
///
/// `unwrap_or(value)` is a floor for a value the carrier could not take back
/// at all, and it is **unreached**: `from_f64` answers `Some` for every
/// quantity this suite submits. It is here so a future corner cannot turn
/// this function into a panic, not because any corner takes it.
fn through_f64(value: Decimal) -> Decimal {
    value.to_f64().and_then(Decimal::from_f64).unwrap_or(value)
}

/// `id eq <id>`: the scope a backend that dropped the argument is left with.
fn only_this_row(id: Uuid) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(id))),
    )
}

// ---------------------------------------------------------------------------
// The own-ledger shape
// ---------------------------------------------------------------------------

/// A ledger of this module's own, for the three defects a wrapper cannot
/// reach.
///
/// Each of those changes a predicate the inner backend owns — which column a
/// range is compared against, which rows a fold walks, whether a batch is
/// decided under one lock — so there is no method to intercept. Everything
/// else mirrors [`InMemoryReferencePlugin`]: the same admission order
/// (duplicate `id` before at-most-one, so an emitter's retry is a replay),
/// the same `from <= window_end < to` selection, the same withdrawal
/// exclusion, the same `(window_end, id)` page order.
///
/// **Two stated ways it is smaller than the reference, neither reachable by
/// a check.** They are named so a reader does not mistake either for a
/// second defect:
///
/// * Its filter evaluator translates exactly the expression shapes the suite
///   dispatches — `And`, `Or`, and `<identifier> eq <literal>` over
///   `tenant_id` and `resource_type` — and admits nothing else. Every scope
///   and every `query.filter` `run_all` sends is one of those, so on every
///   expression the suite produces this evaluator and the reference's agree.
///   The reference's richer disposition (a caller filter it cannot translate
///   refuses the query, a scope it cannot translate excludes the row) has no
///   input here to differ on.
/// * It computes the `SUM`, no-grouping fold and refuses every other shape
///   rather than answering one. That is the only shape `run_all` dispatches.
///   A check that grows a second one gets a loud refusal naming this
///   backend, which is the right failure: a mutant quietly wrong about a
///   fold no row of the matrix accounts for would be wrong in two ways.
///
/// # The mirror is pinned, and how far
///
/// A hand-written mirror of an exemplar usually rots quietly. This one does
/// not, and the matrix is what holds it: every check `run_all` runs is
/// *passed* by at least one subject built on this type. So if the
/// reference's selection predicate, page order, admission decision or fold
/// exclusion changed and the checks moved with it, this mirror would keep
/// the old behaviour, some row would report a violation its expected set
/// does not name, and `assert_eq!(failed, expected)` would fire. Drift
/// between the two implementations is a test failure rather than a thing a
/// reader has to notice.
///
/// **What that does not cover is any behaviour no check asserts in a fresh
/// run**, because a behaviour nothing exercises cannot fail here. The
/// admission *order* is the live example: [`decide`] tries the duplicate
/// `id` branch before the at-most-one branch, matching the reference, and
/// nothing in a single run resubmits an invalidation verbatim to tell the
/// two orders apart. Swap those branches in `reference.rs` and this mirror
/// would silently diverge. It is harmless exactly because nothing depends
/// on it, which is the honest version of the claim rather than "it mirrors
/// the reference".
struct MutantLedger {
    /// The entries admitted so far.
    entries: Mutex<Vec<UsageRecord>>,
    /// Which rule this subject breaks.
    defect: Defect,
}

impl MutantLedger {
    /// An empty ledger carrying one defect.
    fn new(defect: Defect) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            defect,
        }
    }

    /// Borrows the ledger, lifting a poisoned lock the way the reference
    /// does.
    fn ledger(&self) -> Result<MutexGuard<'_, Vec<UsageRecord>>, UsageCollectorPluginError> {
        self.entries.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's ledger lock is poisoned")
        })
    }

    /// Whether one entry is inside a read path's selection.
    ///
    /// The one line the [`Defect::SelectsOnWindowStart`] subject changes is
    /// which bound the range is compared against. Everything else — the
    /// meter, the filter, the metadata predicates — is the reference's.
    fn selects(
        &self,
        entry: &UsageRecord,
        gts_type_id: &MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> bool {
        let bound = if self.defect == Defect::SelectsOnWindowStart {
            entry.window_start
        } else {
            entry.window_end
        };
        let filter_admits = match query.filter() {
            Some(filter) => expr_admits(entry, filter),
            None => true,
        };
        filter_admits
            && entry.gts_type_id == *gts_type_id
            && time_range.contains_window_end(bound)
            && metadata_admits(entry, metadata_filter)
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for MutantLedger {
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let mut ledger = self.ledger()?;
        admit(&mut ledger, record)
    }

    /// The batch, decided under one lock or against the state it began in.
    ///
    /// [`Defect::ChecksThenInsertsTheInvalidation`] is the second: every
    /// entry is decided against a snapshot taken before any of them was
    /// inserted, and the survivors are appended afterwards. That is a
    /// backend that reads for an existing invalidation and then inserts
    /// without holding the two together, and two withdrawals of one record
    /// arriving in one call both observe an un-invalidated target.
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
        if self.defect != Defect::ChecksThenInsertsTheInvalidation {
            return Ok(records
                .into_iter()
                .map(|record| admit(&mut ledger, record))
                .collect());
        }

        let snapshot = ledger.clone();
        let mut pending = Vec::new();
        let mut outcomes = Vec::new();
        for record in records {
            match decide(&snapshot, &record) {
                Ok(Some(stored)) => outcomes.push(Ok(stored)),
                Ok(None) => {
                    pending.push(record.clone());
                    outcomes.push(Ok(record));
                }
                Err(err) => outcomes.push(Err(err)),
            }
        }
        ledger.extend(pending);
        Ok(outcomes)
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        ledger
            .iter()
            .find(|entry| entry.id == id && expr_admits(entry, scope))
            .cloned()
            .ok_or(UsageCollectorPluginError::UsageRecordNotFound { id })
    }

    /// The fold over the selected set.
    ///
    /// [`Defect::FoldsTheInvalidation`] drops one conjunct: the withdrawn
    /// record is still left out, and the invalidation that withdraws it is
    /// counted. Because an invalidation echoes the quantity it withdraws
    /// rather than negating it, the echoed term stays in the sum with
    /// nothing to pair against and the withdrawn measurement is
    /// double-counted.
    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        if !matches!(fold, AggregationFold::Sum) || !group_by.is_empty() {
            return Err(UsageCollectorPluginError::internal(
                "this contract-suite mutant models only the SUM fold with no grouping, which is \
                 the one shape the suite dispatches; a check that dispatches another shape needs \
                 the mutant extended rather than a wrong answer invented for it",
            ));
        }
        let ledger = self.ledger()?;
        let withdrawn = withdrawn_targets(&ledger);
        let counts_invalidations = self.defect == Defect::FoldsTheInvalidation;
        let mut total = BigDecimal::from(0);
        let mut rows = 0_usize;
        for entry in ledger.iter() {
            let folded = self.selects(entry, &gts_type_id, time_range, query, metadata_filter)
                && (counts_invalidations || entry.invalidation.is_none())
                && !withdrawn.contains(&entry.id);
            if folded {
                total += widen(entry.value)?;
                rows += 1;
            }
        }
        Ok(AggregationResult {
            buckets: vec![AggregationBucket {
                key: Vec::new(),
                value: if rows == 0 { None } else { Some(total) },
            }],
        })
    }

    async fn list_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let mut items: Vec<UsageRecord> = ledger
            .iter()
            .filter(|entry| self.selects(entry, &gts_type_id, time_range, query, metadata_filter))
            .cloned()
            .collect();
        items.sort_by(|left, right| {
            left.window_end
                .cmp(&right.window_end)
                .then_with(|| left.id.cmp(&right.id))
        });
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
}

/// Whether one entry may be admitted, without admitting it.
///
/// `Ok(Some(stored))` is an idempotent replay of an entry already held,
/// `Ok(None)` is "insert it", `Err` is a refusal. Split out from [`admit`]
/// so [`Defect::ChecksThenInsertsTheInvalidation`] can decide a whole batch
/// against one snapshot — the deciding and the inserting are the two halves
/// that defect pulls apart.
///
/// The duplicate-`id` branch runs first, as the reference's does: an
/// invalidation resubmitted verbatim collides on its own `id` and is a
/// replay, not a second withdrawal of its target.
fn decide(
    ledger: &[UsageRecord],
    record: &UsageRecord,
) -> Result<Option<UsageRecord>, UsageCollectorPluginError> {
    if let Some(stored) = ledger.iter().find(|entry| entry.id == record.id) {
        if stored == record {
            return Ok(Some(stored.clone()));
        }
        return Err(UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key: record.idempotency_key.as_str().to_owned(),
            existing_id: stored.id,
        });
    }
    if let Some(invalidation) = record.invalidation.as_ref()
        && let Some(existing) = ledger.iter().find(|entry| {
            entry
                .invalidation
                .as_ref()
                .is_some_and(|other| other.target == invalidation.target)
        })
    {
        return Err(UsageCollectorPluginError::AlreadyInvalidated {
            id: invalidation.target,
            invalidated_by: existing.id,
        });
    }
    Ok(None)
}

/// Decides one entry against the ledger and inserts it if it may be.
fn admit(
    ledger: &mut Vec<UsageRecord>,
    record: UsageRecord,
) -> Result<UsageRecord, UsageCollectorPluginError> {
    if let Some(stored) = decide(ledger, &record)? {
        return Ok(stored);
    }
    ledger.push(record.clone());
    Ok(record)
}

/// Every `UsageRecord.id` an accepted invalidation names.
fn withdrawn_targets(ledger: &[UsageRecord]) -> BTreeSet<Uuid> {
    ledger
        .iter()
        .filter_map(|entry| entry.invalidation.as_ref().map(|inv| inv.target))
        .collect()
}

/// Whether an entry satisfies every metadata filter in the slice.
fn metadata_admits(entry: &UsageRecord, filters: &[MetadataFilter]) -> bool {
    filters.iter().all(|filter| {
        entry
            .metadata
            .get(filter.key())
            .is_some_and(|value| filter.values().iter().any(|candidate| candidate == value))
    })
}

/// Whether an entry satisfies a filter expression.
///
/// The vocabulary is exactly what the suite dispatches; see
/// [`MutantLedger`] for why that is a stated limit rather than a second
/// defect. Anything outside it admits nothing, which is the reference's
/// disposition for a scope it cannot read.
fn expr_admits(entry: &UsageRecord, expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::And(left, right) => expr_admits(entry, left) && expr_admits(entry, right),
        ast::Expr::Or(left, right) => expr_admits(entry, left) || expr_admits(entry, right),
        ast::Expr::Compare(left, ast::CompareOperator::Eq, right) => match (&**left, &**right) {
            (ast::Expr::Identifier(name), ast::Expr::Value(value)) => {
                field_matches(entry, name, value)
            }
            _ => false,
        },
        _ => false,
    }
}

/// Compares one record attribute against a literal.
///
/// Two identifiers, because two is what the suite dispatches: `tenant_id`
/// compiled to a UUID literal, and `resource_type` compiled to a string one.
/// Nothing wider is written here on the chance a check might want it — an
/// arm no dispatch reaches is untested code inside a subject whose whole job
/// is to be wrong in one known place.
fn field_matches(entry: &UsageRecord, name: &str, value: &ast::Value) -> bool {
    match (name, value) {
        ("tenant_id", ast::Value::Uuid(want)) => entry.tenant_id == *want,
        ("resource_type", ast::Value::String(want)) => entry.resource_ref.resource_type() == want,
        _ => false,
    }
}

/// Widens a quantity to the aggregate surface's carrier, through the decimal
/// rendering, which is exact for every [`Decimal`].
fn widen(value: Decimal) -> Result<BigDecimal, UsageCollectorPluginError> {
    BigDecimal::from_str(&value.to_string()).map_err(|err| {
        UsageCollectorPluginError::internal(format!(
            "a persisted quantity could not be widened to the aggregate carrier: {err}"
        ))
    })
}
