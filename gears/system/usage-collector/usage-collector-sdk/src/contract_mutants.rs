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
//! *Behaviourally* is the exact word. Five subjects wrap a real reference
//! backend and are that backend plus one interception; the other two
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
//! strongest form the subject can take. Five defects fit (quantity,
//! period-blind dedup, point-read scope, and the two withdrawal defects): one
//! rewrites the quantity on the way in, one keeps a dedup index beside the
//! ledger, one substitutes the scope on the point read, and two rewrite how a
//! second withdrawal of a record is answered.
//!
//! **A ledger of its own** ([`MutantLedger`]) is needed by the other two
//! (selection column, fold exclusion), because each changes a predicate the
//! inner backend owns and no interception can reach it: which column a range
//! meets and which rows a fold walks. It mirrors the
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
use crate::feed::{FeedPage, FeedPosition, FeedStart};
use crate::models::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult, MetadataFilter,
    MeterTypeId, UsageRecord,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::reconciliation::ReconciliationMetadata;
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
    /// Keys dedup on `(tenant, type, idempotency_key, entry_type)`, omitting
    /// the two covered-period bounds — the derived identity with the period
    /// struck out.
    DedupIgnoresThePeriod,
    /// Excludes the withdrawn record from the fold but folds the
    /// invalidation. DESIGN names this one: it double-counts the withdrawn
    /// measurement.
    FoldsTheInvalidation,
    /// Absorbs a second withdrawal of a record even under another reason code —
    /// the mistake a backend makes by comparing only the dedup identity.
    AbsorbsAWithdrawalWithAnotherReason,
    /// Refuses a second withdrawal of a record even under the same reason code —
    /// the mistake a backend makes by keeping an at-most-one rule of its own.
    RefusesAWithdrawalWithTheSameReason,
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
        | Defect::IgnoresScopeOnThePointRead
        | Defect::AbsorbsAWithdrawalWithAnotherReason
        | Defect::RefusesAWithdrawalWithTheSameReason => Box::new(WrappedReference::new(defect)),
        Defect::SelectsOnWindowStart | Defect::FoldsTheInvalidation => {
            Box::new(MutantLedger::new(defect))
        }
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
/// One qualification, and it holds for all five wrapped defects:
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
    /// on: `(tenant_id, gts_type_id, idempotency_key, entry_type)` to the
    /// entry that claimed it. Unused by the other four defects.
    ///
    /// A claim is recorded when the entry is admitted rather than after the
    /// inner backend stores it, which is a unique index written inside the
    /// same transaction. It therefore leaves a claim behind for an entry the
    /// inner backend then refuses, and `at-most-one-invalidation` submits
    /// **two** such entries: in its separate-call pair and in its one-batch
    /// pair, the withdrawal that differs from the accepted one by reason code
    /// alone. Both are unobservable here: each carries the accepted
    /// withdrawal's derived `id`, so the claim it overwrites names the same
    /// identity, and neither key is resubmitted over a second period — the
    /// only question this index is ever asked.
    period_blind_keys: Mutex<BTreeMap<PeriodBlindKey, UsageRecord>>,
}

/// The four inputs a period-blind dedup identity keys on:
/// `(tenant_id, gts_type_id, idempotency_key, entry_type)`, the entry type as
/// the lowercase wire literal the derivation itself reads
/// ([`crate::models::EntryType::as_str`]).
///
/// Named for what it leaves out. The derived identity reads six inputs, and
/// the two missing here are the covered-period bounds — which is the whole of
/// [`Defect::DedupIgnoresThePeriod`].
type PeriodBlindKey = (Uuid, String, String, &'static str);

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
                quantity: UsageQuantity::try_from(through_f64(record.quantity.as_decimal()))
                    .expect(
                        "this mutant's float round-trip stays within the published quantity \
                         range for every corner the suite submits",
                    ),
                ..record
            }),
            Defect::DedupIgnoresThePeriod => self.claim_period_blind_key(record),
            // Enumerated rather than caught by a wildcard. A new defect
            // routed here and forgotten would otherwise pass its
            // entries through untouched and report no violation at all;
            // spelling the variants out makes that a compile error instead
            // of a matrix row whose subject does nothing. The point read's
            // defect is applied on the read path, the two withdrawal defects around
            // the inner call, and the last two never
            // reach this type at all — `mutant` routes them to
            // `MutantLedger` — but exhaustiveness is the whole point.
            Defect::IgnoresScopeOnThePointRead
            | Defect::AbsorbsAWithdrawalWithAnotherReason
            | Defect::RefusesAWithdrawalWithTheSameReason
            | Defect::SelectsOnWindowStart
            | Defect::FoldsTheInvalidation => Ok(record),
        }
    }

    /// Admits an entry only if no other entry already holds its
    /// `(tenant, type, idempotency_key, entry_type)`.
    ///
    /// This is a unique index over the derived identity with the covered
    /// period struck out, and that omission is the whole of the defect: the
    /// two bounds are inputs to the derived identity and invisible to this
    /// index, so one key over two periods collides here and is one entry
    /// rather than two.
    ///
    /// The entry type is in the index for the opposite reason. A withdrawal
    /// repeats its target's idempotency key, so an index blind to it would
    /// collide every withdrawal with the record it withdraws — a second
    /// mistake, in a subject that must be wrong in exactly one way.
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
            record.entry_type().as_str(),
        );
        let mut claimed = self.period_blind_keys.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's dedup index lock is poisoned")
        })?;
        if let Some(existing) = claimed.get(&key)
            && existing.id != record.id
        {
            return Err(UsageCollectorPluginError::idempotency_conflict(
                record.idempotency_key.as_str(),
                existing.clone(),
            ));
        }
        claimed.insert(key, record.clone());
        Ok(record)
    }

    /// [`Defect::RefusesAWithdrawalWithTheSameReason`]: a withdrawal whose
    /// derived id is already stored is refused, whatever it states.
    async fn refused_as_a_second_withdrawal(
        &self,
        record: &UsageRecord,
    ) -> Option<UsageCollectorPluginError> {
        if self.defect != Defect::RefusesAWithdrawalWithTheSameReason
            || record.invalidation.is_none()
        {
            return None;
        }
        let stored = self
            .inner
            .get_usage_record(record.id, &tenant_scope(record.tenant_id), true)
            .await
            .ok()?;
        Some(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            stored,
        ))
    }

    /// [`Defect::AbsorbsAWithdrawalWithAnotherReason`]: a conflict on a
    /// withdrawal is answered as an absorb of the stored entry.
    fn after_admission(
        &self,
        record: &UsageRecord,
        outcome: Result<UsageRecord, UsageCollectorPluginError>,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        match outcome {
            Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. })
                if self.defect == Defect::AbsorbsAWithdrawalWithAnotherReason
                    && record.invalidation.is_some() =>
            {
                Ok(*existing)
            }
            other => other,
        }
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for WrappedReference {
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let record = self.on_admission(record)?;
        if let Some(refusal) = self.refused_as_a_second_withdrawal(&record).await {
            return Err(refusal);
        }
        let outcome = self.inner.create_usage_record(record.clone()).await;
        self.after_admission(&record, outcome)
    }

    /// The batch, with the defect applied per entry and the survivors handed
    /// to the inner backend in **one** call.
    ///
    /// Passing the survivors as a batch rather than admitting them one at a
    /// time is what keeps the resolution of a later same-identity entry
    /// against an earlier one in the same call the inner backend's to decide.
    /// Admitting them singly would also answer `at-most-one-invalidation`'s
    /// one-batch pair correctly here, and it would mean the other wrapped
    /// subjects passed that half of the check for a reason of the wrapper's
    /// own rather than the exemplar's.
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
        let mut prepared: Vec<Result<UsageRecord, UsageCollectorPluginError>> =
            Vec::with_capacity(records.len());
        for record in records {
            let entry = match self.on_admission(record) {
                Ok(record) => match self.refused_as_a_second_withdrawal(&record).await {
                    Some(refusal) => Err(refusal),
                    None => Ok(record),
                },
                Err(err) => Err(err),
            };
            prepared.push(entry);
        }
        let survivors: Vec<UsageRecord> = prepared
            .iter()
            .filter_map(|entry| entry.as_ref().ok().cloned())
            .collect();
        // A batch every entry of which this wrapper refused must not reach
        // the inner backend: the reference answers an empty batch with
        // `Internal`, and this call would then fail outright rather than
        // reporting the per-entry refusals it already has. Unreachable
        // under the suite as written: two defects refuse entries here,
        // `DedupIgnoresThePeriod` and `RefusesAWithdrawalWithTheSameReason`,
        // and the one batch the suite sends carries two withdrawals of a
        // target that has none stored yet, both repeating that target's
        // idempotency key and so deriving one `id` — the period-blind index
        // admits the second under the claim the first left, and the
        // same-reason refusal finds nothing stored to refuse either against.
        // That is also why the emptiness guard above is repeated rather than
        // left to the inner backend to raise.
        let inner = if survivors.is_empty() {
            Vec::new()
        } else {
            self.inner.create_usage_records(survivors).await?
        };
        let mut inner = inner.into_iter();
        Ok(prepared
            .into_iter()
            .map(|entry| match entry {
                Ok(record) => {
                    let outcome = inner.next().unwrap_or_else(|| {
                        Err(UsageCollectorPluginError::internal(
                            "the inner backend answered fewer outcomes than the batch carried entries",
                        ))
                    });
                    self.after_admission(&record, outcome)
                }
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
        converged_only: bool,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if self.defect == Defect::IgnoresScopeOnThePointRead {
            return self
                .inner
                .get_usage_record(id, &only_this_row(id), converged_only)
                .await;
        }
        self.inner.get_usage_record(id, scope, converged_only).await
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

    /// Delegated whole. No defect routed to this wrapper touches the feed.
    async fn read_feed_page(
        &self,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition>, UsageCollectorPluginError> {
        self.inner
            .read_feed_page(subscription, scope, start, until, limit)
            .await
    }

    /// Delegated whole. No defect routed to this wrapper touches
    /// reconciliation.
    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        self.inner
            .get_reconciliation_metadata(tenant_id, gts_type_id, time_range, fold, scope)
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

/// `tenant_id eq <tenant>`: the scope a mutant reads its own ledger under.
fn tenant_scope(tenant: Uuid) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(tenant))),
    )
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

/// A ledger of this module's own, for the two defects a wrapper cannot
/// reach.
///
/// Each of those changes a predicate the inner backend owns — which column a
/// range meets and which rows a fold walks — so there is no method to
/// intercept. Everything else mirrors [`InMemoryReferencePlugin`]: the same
/// admission decision (dedup by caller-supplied fields), the same `from <= window_end < to` selection, the same withdrawal
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

    /// The batch, decided under one lock in input order.
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

    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        _converged_only: bool,
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
                total += widen(entry.quantity.as_decimal())?;
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

    /// Unmodelled: this mirror reproduces the reference's write and fold
    /// paths alone, and every defect routed here breaks one of those.
    ///
    /// `Internal` rather than a wrong page, because no check in this slice
    /// points a feed read at this subject and an invented answer would make a
    /// future one fail for a reason no matrix row names.
    async fn read_feed_page(
        &self,
        _subscription: &[MeterTypeId],
        _scope: &ast::Expr,
        _start: FeedStart<FeedPosition>,
        _until: Option<FeedPosition>,
        _limit: u64,
    ) -> Result<FeedPage<FeedPosition>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "this contract-suite mutant models the write and fold paths only and serves no feed \
             page; a check that reads the feed needs the mutant extended rather than a wrong \
             answer invented for it",
        ))
    }

    /// Unmodelled, for the same reason [`Self::read_feed_page`] is.
    async fn get_reconciliation_metadata(
        &self,
        _tenant_id: Uuid,
        _gts_type_id: MeterTypeId,
        _time_range: TimeRange,
        _fold: AggregationFold,
        _scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "this contract-suite mutant models the write and fold paths only and reports no \
             reconciliation metadata; a check that reads it needs the mutant extended rather \
             than a wrong answer invented for it",
        ))
    }
}

/// Whether one entry may be admitted, without admitting it.
///
/// `Ok(Some(stored))` is an idempotent replay of an entry already held,
/// `Ok(None)` is "insert it", `Err` is a refusal. The decision is the
/// reference's: a collision on `id` resolves by caller-supplied fields.
fn decide(
    ledger: &[UsageRecord],
    record: &UsageRecord,
) -> Result<Option<UsageRecord>, UsageCollectorPluginError> {
    match ledger.iter().find(|entry| entry.id == record.id) {
        Some(stored) if stored.caller_supplied_eq(record) => Ok(Some(stored.clone())),
        Some(stored) => Err(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            stored.clone(),
        )),
        None => Ok(None),
    }
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
