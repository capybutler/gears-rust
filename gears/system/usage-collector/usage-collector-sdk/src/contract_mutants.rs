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
//! reference where the defect is not, and it is smaller in one stated way
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
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult,
    MAX_AGGREGATION_BUCKETS, MetadataFilter, MeterTypeId, UsageRecord,
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

/// One admitted entry and the sequence the feed orders it by.
///
/// The mirror of the reference backend's own entry. A sequence is stamped at
/// admission, is unique across the ledger's whole life, and ascends in
/// admission order, so a position issued over it goes on denoting the same
/// point in the feed's order however the entries around it change.
#[derive(Debug)]
struct Entry {
    /// The sequence stamped when this entry was admitted.
    sequence: u64,
    /// The entry as persisted.
    record: UsageRecord,
}

/// The ledger, its sequence counter and its retention marks.
#[derive(Debug, Default)]
struct Ledger {
    /// Admitted entries in admission order, so their sequences ascend.
    entries: Vec<Entry>,
    /// The highest sequence stamped so far. It only ever rises, and a
    /// sequence is never reissued.
    ///
    /// Held apart from `entries.len()` for the reference's reason: a length
    /// renumbers every entry after one that is removed, and a position already
    /// issued has to go on denoting the same point in the feed's order.
    stamped: u64,
    /// Per GTS type, the highest sequence retention has removed, keyed by the
    /// type's wire string because [`MeterTypeId`] implements no `Ord`.
    ///
    /// **Nothing raises a mark here yet.** A mark is raised by a sweep, a
    /// sweep is driven through
    /// [`ContractRetention`](super::retention::ContractRetention), and this
    /// type does not implement that trait — the impl lands with the
    /// retention-driven entry point the matrix does not yet have. Until then
    /// this map stays empty and the refusal in [`MutantLedger`]'s `read_feed_page`
    /// never fires, which is exactly the answer the reference gives for a
    /// backend nobody has driven.
    retention_marks: BTreeMap<String, u64>,
}

impl Ledger {
    /// Stamps one record with the next sequence and appends it.
    fn push(&mut self, record: UsageRecord) {
        self.stamped = self.stamped.saturating_add(1);
        self.entries.push(Entry {
            sequence: self.stamped,
            record,
        });
    }

    /// The admitted records in admission order.
    ///
    /// Every path but the feed reads the ledger through this, as in the
    /// reference: only the feed has a position to seek to, so only it reads
    /// the sequences.
    fn records(&self) -> impl Iterator<Item = &UsageRecord> {
        self.entries.iter().map(|entry| &entry.record)
    }

    /// Whether retention has removed an entry of a subscribed type strictly
    /// after `position`.
    ///
    /// Strict, as in the reference: a position naming the highest sequence a
    /// type has lost has lost nothing *after* itself, and its continuation is
    /// intact.
    fn retention_has_passed(&self, subscription: &[MeterTypeId], position: u64) -> bool {
        subscription.iter().any(|gts_type_id| {
            self.retention_marks
                .get(gts_type_id.as_str())
                .is_some_and(|mark| *mark > position)
        })
    }
}

/// A ledger of this module's own, for the two defects a wrapper cannot
/// reach.
///
/// Each of those changes a predicate the inner backend owns — which column a
/// range meets and which rows a fold walks — so there is no method to
/// intercept. Everything else mirrors [`InMemoryReferencePlugin`]: the same
/// admission decision (dedup by caller-supplied fields), the same
/// `from <= window_end < to` selection, the same withdrawal exclusion, the
/// same `(window_end, id)` ledger page order, the same sequence-stamped feed
/// with its seek, its scanned-entry cursor and its retention refusal, the
/// same grouping and fold values, and the same reconciliation counters and
/// watermarks.
///
/// **One stated way it is smaller than the reference, and no check reaches
/// it.** It is named so a reader does not mistake it for a second defect:
/// its filter evaluator translates exactly the expression shapes the suite
/// dispatches — `And`, `Or`, and `<identifier> eq <literal>` over `tenant_id`
/// and `resource_type` — and admits nothing else. Every scope and every
/// `query.filter` [`run_all`](super::run_all) sends is one of those, so on
/// every expression the suite produces this evaluator and the reference's
/// agree. The reference's richer disposition (a caller filter it cannot
/// translate refuses the query, a scope it cannot translate excludes the row)
/// has no input here to differ on.
///
/// # The mirror is pinned, and how far
///
/// A hand-written mirror of an exemplar usually rots quietly. This one does
/// not, and the matrix is what holds it: every check
/// [`run_all`](super::run_all) runs is *passed* by at least one subject built
/// on this type. So if a mirrored behaviour changed in the reference and the
/// checks moved with it, this mirror would keep the old behaviour, some row
/// would report a violation its expected set does not name, and
/// `assert_eq!(failed, expected)` would fire. Drift is a test failure rather
/// than a thing a reader has to notice.
///
/// **The pin reaches exactly what `run_all` dispatches, and no further**,
/// which is less than the whole mirror. Pinned today: the covered-period
/// bound the selection meets, the admission decision, the withdrawal
/// exclusion, the ungrouped `SUM` the fold check reads, and which rows the
/// three scope-carrying read paths answer with. Each was measured by breaking
/// it and watching a row grow, not inferred from the check list.
///
/// Everything else here is level with the reference and **unpinned**: the
/// ledger page's *order* (its membership is pinned, its sort is asserted by
/// no check), the feed page with its seek, its cursor rule and its retention
/// refusal, the grouped and non-`SUM` folds, and the reconciliation figures.
/// They are mirrored because the checks that read them are coming, and each
/// becomes pinned by the check that first dispatches it. An unpinned
/// behaviour is where this mirror can still rot in silence, which is the
/// argument for keeping it level now rather than letting it answer
/// `Internal` until someone needs it.
struct MutantLedger {
    /// The entries admitted so far, with the sequences the feed orders them
    /// by.
    entries: Mutex<Ledger>,
    /// Which rule this subject breaks.
    defect: Defect,
}

impl MutantLedger {
    /// An empty ledger carrying one defect.
    fn new(defect: Defect) -> Self {
        Self {
            entries: Mutex::new(Ledger::default()),
            defect,
        }
    }

    /// Borrows the ledger, lifting a poisoned lock the way the reference
    /// does.
    fn ledger(&self) -> Result<MutexGuard<'_, Ledger>, UsageCollectorPluginError> {
        self.entries.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's ledger lock is poisoned")
        })
    }

    /// Encodes the sequence of the last entry scanned as a feed position:
    /// eight big-endian bytes, so bytewise ordering matches numeric ordering,
    /// which is the reference's own encoding.
    fn encode_position(sequence: u64) -> Result<FeedPosition, UsageCollectorPluginError> {
        FeedPosition::new(sequence.to_be_bytes().to_vec()).map_err(|err| {
            UsageCollectorPluginError::internal(format!(
                "the mutant ledger could not encode its own feed position: {err}"
            ))
        })
    }

    /// Decodes a position this backend issued.
    fn decode_position(position: &FeedPosition) -> Result<u64, UsageCollectorPluginError> {
        let bytes: [u8; 8] = position.as_bytes().try_into().map_err(|_| {
            UsageCollectorPluginError::internal(format!(
                "the mutant ledger issues eight-byte feed positions and was handed {} bytes",
                position.len()
            ))
        })?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// The covered-period bound a range is compared against.
    ///
    /// **The one line [`Defect::SelectsOnWindowStart`] changes**, and it is a
    /// method rather than an expression inside [`Self::selects`] because a
    /// backend that ported the pre-period point-in-time column meets every
    /// range on it: the read paths' selection and the reconciliation figures
    /// alike. A subject honest on one and wrong on the other would be a
    /// backend nobody writes.
    fn range_bound(&self, entry: &UsageRecord) -> time::OffsetDateTime {
        if self.defect == Defect::SelectsOnWindowStart {
            entry.window_start
        } else {
            entry.window_end
        }
    }

    /// Whether one entry is inside a read path's selection.
    ///
    /// Everything but the bound — the meter, the filter, the metadata
    /// predicates — is the reference's; the bound is [`Self::range_bound`]'s.
    fn selects(
        &self,
        entry: &UsageRecord,
        gts_type_id: &MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> bool {
        let filter_admits = match query.filter() {
            Some(filter) => expr_admits(entry, filter),
            None => true,
        };
        filter_admits
            && entry.gts_type_id == *gts_type_id
            && time_range.contains_window_end(self.range_bound(entry))
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
            .records()
            .find(|entry| entry.id == id && expr_admits(entry, scope))
            .cloned()
            .ok_or(UsageCollectorPluginError::UsageRecordNotFound { id })
    }

    /// The fold over the selected set, grouped and folded as the reference
    /// groups and folds.
    ///
    /// [`Defect::FoldsTheInvalidation`] drops one conjunct: the withdrawn
    /// record is still left out, and the invalidation that withdraws it is
    /// counted. Because an invalidation echoes the quantity it withdraws
    /// rather than negating it, the echoed term stays in the sum with
    /// nothing to pair against and the withdrawn measurement is
    /// double-counted.
    ///
    /// Everything after the row filter — the grouping, the bucket cap, the
    /// split an empty selection makes by fold — is [`fold_rows`]'s, which
    /// mirrors the reference's function of the same name. Answering only one
    /// fold shape here, as this subject once did, would fail a check widened
    /// to a second shape for a reason neither defect is about.
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
        let counts_invalidations = self.defect == Defect::FoldsTheInvalidation;
        let mut rows: Vec<&UsageRecord> = Vec::new();
        for entry in ledger.records() {
            if self.selects(entry, &gts_type_id, time_range, query, metadata_filter)
                && (counts_invalidations || entry.invalidation.is_none())
                && !withdrawn.contains(&entry.id)
            {
                rows.push(entry);
            }
        }
        fold_rows(fold, &rows, group_by)
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
            .records()
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

    /// A page from the ledger's own append order, mirroring the reference's.
    ///
    /// The three properties a later feed check reads are all here. The
    /// resumption is a **seek**: `sequence > from` resumes at the first entry
    /// the position does not already cover, never a count of entries walked
    /// past, which is what DESIGN §3.3's plugin obligations put on a real
    /// plugin in stating that *"Offset/limit scans are forbidden on both
    /// paginated paths"*. The cursor advances past every entry **scanned**
    /// rather than every entry **admitted**, so a position denotes a prefix of
    /// the ledger and the same prefix under every grant — the property
    /// `a_feed_position_denotes_the_same_ledger_prefix_under_every_grant`
    /// pins for the reference and nothing pins here yet. And a cursor whose
    /// continuation retention has truncated is refused, which is DESIGN §3.3's
    /// `feed-retention-refusal`, though no mark can rise until this type is
    /// drivable.
    ///
    /// Neither defect routed here touches the feed. This method is a mirror
    /// and nothing more: a wrong answer invented for it would fail a future
    /// check for a reason no matrix row names.
    async fn read_feed_page(
        &self,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition>, UsageCollectorPluginError> {
        // A zero-limit page emits nothing and so advances its cursor past
        // nothing, which makes `next` a fixpoint. The published limit is at
        // least one, so a zero one is a host-contract breach.
        if limit == 0 {
            return Err(UsageCollectorPluginError::internal(
                "read_feed_page was called with a zero limit (host-contract breach): the \
                 published page limit is at least one, and a zero-limit page cannot advance its \
                 own cursor",
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
        for entry in ledger.entries.iter().filter(|entry| entry.sequence > from) {
            // `upper` names an entry a bounded replay is still asked to read,
            // so the replay stops at the first entry beyond it.
            if entry.sequence > upper || u64::try_from(entries.len()).unwrap_or(u64::MAX) >= limit {
                break;
            }
            // The cursor moves onto this entry's sequence whether or not the
            // subscription and the scope admit it. Advancing only past
            // admitted entries would make the position depend on who asked.
            cursor = entry.sequence;
            if subscription.contains(&entry.record.gts_type_id) && expr_admits(&entry.record, scope)
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

    /// Counters, the declared fold, and watermarks over the ledger, mirroring
    /// the reference's.
    ///
    /// The compiled `scope` applies **before** the tenant and type arguments,
    /// so a tenant it excludes answers exactly as one holding no entries. The
    /// two figures over the range disagree on purpose: DESIGN §3.3's plugin
    /// obligations have `accepted_count` count *"every accepted entry the
    /// range selects, invalidations included, because it reports ingestion
    /// activity rather than aggregating the meter; the quantity summary
    /// excludes withdrawn pairs"*.
    ///
    /// The range meets [`MutantLedger::range_bound`] rather than `window_end`
    /// directly, so [`Defect::SelectsOnWindowStart`] reaches this path as it
    /// reaches every other range. The four passes are the reference's shape
    /// too, and its doc says why not to copy them into a real backend.
    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let withdrawn = withdrawn_targets(&ledger);
        let in_scope = || {
            ledger
                .records()
                .filter(|entry| expr_admits(entry, scope))
                .filter(|entry| entry.tenant_id == tenant_id && entry.gts_type_id == gts_type_id)
        };
        let in_range =
            || in_scope().filter(|entry| time_range.contains_window_end(self.range_bound(entry)));

        let accepted_count = in_range().count();
        let folded: Vec<&UsageRecord> = in_range()
            .filter(|entry| entry.invalidation.is_none() && !withdrawn.contains(&entry.id))
            .collect();

        Ok(ReconciliationMetadata {
            accepted_count: u64::try_from(accepted_count).unwrap_or(u64::MAX),
            quantity_summary: fold_value(fold, &folded)?,
            max_accepted_at: in_scope().map(|entry| entry.accepted_at).max(),
            max_window_end: in_scope().map(|entry| entry.window_end).max(),
        })
    }
}

/// Whether one entry may be admitted, without admitting it.
///
/// `Ok(Some(stored))` is an idempotent replay of an entry already held,
/// `Ok(None)` is "insert it", `Err` is a refusal. The decision is the
/// reference's: a collision on `id` resolves by caller-supplied fields.
fn decide(
    ledger: &Ledger,
    record: &UsageRecord,
) -> Result<Option<UsageRecord>, UsageCollectorPluginError> {
    match ledger.records().find(|entry| entry.id == record.id) {
        Some(stored) if stored.caller_supplied_eq(record) => Ok(Some(stored.clone())),
        Some(stored) => Err(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            stored.clone(),
        )),
        None => Ok(None),
    }
}

/// Decides one entry against the ledger and inserts it if it may be,
/// stamping it with the next sequence.
fn admit(
    ledger: &mut Ledger,
    record: UsageRecord,
) -> Result<UsageRecord, UsageCollectorPluginError> {
    if let Some(stored) = decide(ledger, &record)? {
        return Ok(stored);
    }
    ledger.push(record.clone());
    Ok(record)
}

/// Every `UsageRecord.id` an accepted invalidation names.
fn withdrawn_targets(ledger: &Ledger) -> BTreeSet<Uuid> {
    ledger
        .records()
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

/// Groups the selected rows and folds each group, mirroring the reference's
/// function of the same name.
///
/// An empty `group_by` is the no-grouping case: exactly one bucket carrying an
/// empty key, emitted even over an empty selection, and what that bucket
/// carries is [`fold_value`]'s to decide. A grouping empties differently: the
/// groups are keyed from the surviving rows alone, so a group nothing survives
/// in is never keyed and yields no bucket. DESIGN §3.3's plugin obligations
/// state both halves — *"A grouped query yields no bucket for a group nothing
/// survives in."*
///
/// The bucket count is capped at [`MAX_AGGREGATION_BUCKETS`] `+ 1`, one past
/// the cap, which is what lets a gateway tell "at the cap" from "over it".
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
/// A row with no subject is excluded from a `subject_id` grouping rather than
/// bucketed under an empty string, and the same holds for `subject_type` and
/// for a metadata key the row does not carry. The reference's module docs
/// argue under "Stated limits" why that stands; this is the mirror of it, not
/// a second opinion.
fn bucket_key(row: &UsageRecord, group_by: &[AggregationDimension]) -> Option<Vec<String>> {
    group_by
        .iter()
        .map(|dimension| match dimension {
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
/// **An empty bucket splits by fold.** DESIGN §3.3's plugin obligations:
/// *"`SUM` and `COUNT` are defined over an empty selection and report `0`;
/// `MAX`, `MIN` and `LATEST` are not and report absent"*. So `SUM` returns its
/// accumulator, which starts at zero and has nothing added to it, and `COUNT`
/// returns the row count; the other three have no row to read a quantity off
/// and answer `None`. This mirror used to answer absent for an empty `SUM`,
/// which was the reference's own answer before it was brought to DESIGN.
///
/// **`LATEST` takes the greatest `(window_end, accepted_at, id)`**, the whole
/// of DESIGN §3.1's declared order. All three keys are read, and `id` is
/// compared as bytes by [`Uuid`]'s derived `Ord`, so the order is total and
/// the answer never depends on ledger insertion order.
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
                total += widen(row.quantity.as_decimal())?;
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
        .map(|row| widen(row.quantity.as_decimal()))
        .transpose()
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
