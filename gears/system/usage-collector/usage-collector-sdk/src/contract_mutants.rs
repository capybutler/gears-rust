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
//! *Behaviourally* is the exact word. Nine subjects wrap a real reference
//! backend and are that backend plus one interception; the other four
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
//! strongest form the subject can take. Nine defects fit (quantity, the
//! store's own acceptance instant, a defaulted origin, period-blind dedup,
//! entry-type-blind dedup, the entry-type-blind conflict read-back,
//! point-read scope, and the two withdrawal defects): three rewrite a field
//! on the way in, three keep an index beside the ledger — two to refuse an
//! admission, one only to decide which stored entry a collision is answered
//! with — one substitutes the scope on the point read, and two rewrite how a
//! second withdrawal of a record is answered.
//!
//! **A ledger of its own** ([`MutantLedger`]) is needed by the other four
//! (selection column, fold exclusion, missing unique constraint, missing
//! in-batch dedup map), because each changes something the inner backend owns
//! and no interception can reach it: which column a range meets, which rows a
//! fold walks, what admission writes, and what a batch's own rows are decided
//! against. It mirrors the reference where the defect is not, and it is
//! smaller in one stated way that no check reaches — see [`MutantLedger`].
//!
//! # DESIGN's three identity sites, and the subject for each
//!
//! §3.3's six-part-identity obligation names three places identity has to be
//! enforced: *"Everything keyed on identity — a unique constraint or conflict
//! target, the read-back of a conflicting entry, an in-batch dedup map —
//! includes `entry_type` or keys on `id`, which covers all six inputs."* All
//! three now have a subject, which is worth stating because it is the reason
//! two of them exist:
//!
//! * **The unique constraint** — [`Defect::DedupIgnoresTheEntryType`] keys it
//!   on five of the six inputs, and [`Defect::LedgerHasNoUniqueConstraint`]
//!   leaves it out altogether.
//! * **The read-back of a conflicting entry** —
//!   [`Defect::ConflictReadBackIgnoresTheEntryType`].
//! * **The in-batch dedup map** —
//!   [`Defect::BatchResolvesAgainstThePreCallLedger`], which has none at all.
//!
//! A further subject for a site that already has one buys less than one for a
//! site that has none, and the gap recorded on
//! [`Defect::LedgerHasNoUniqueConstraint`] is left open on exactly that
//! reasoning.
//!
//! # The four server-assigned fields, and which two have a subject
//!
//! `server-field-round-trip` asserts `id`, `accepted_at`, `origin` and an
//! invalidation's `invalidates`. Two of the four have a subject here and two
//! do not, which bounds what that check's matrix row establishes. The split
//! is deliberate rather than unfinished:
//!
//! * **`accepted_at`** — [`Defect::StampsItsOwnAcceptedAt`]. DESIGN names the
//!   defect outright.
//! * **`origin`** — [`Defect::DefaultsOriginToLive`]. DESIGN's fidelity row
//!   names defaulting outright, and the check's fixtures hand the plugin both
//!   origins precisely so a backend that answers one of them always is caught.
//! * **`id`** — **no subject, and none is wanted.** DESIGN's rule forbids
//!   re-deriving, and re-deriving `id` is a no-op: the derivation is a `UUIDv5`
//!   over the six identity inputs, deterministic, so a backend that recomputes
//!   it from the row it stored arrives at the same value. A subject would have
//!   to assign a *surrogate* identity instead — a `bigserial`, a fresh v4 —
//!   which is a different mistake from the one this row states and a caricature
//!   of it besides, since such a backend loses the dedup identity and fails
//!   most of the suite. The reasoning is the artefact; the subject would not be.
//! * **`invalidates`** — **no subject yet, and it cannot be confined to one
//!   check.** A backend that dropped the reference on read also breaks the
//!   withdrawal exclusion: [`withdrawn_targets`] and the reference's function
//!   of the same name read exactly that field to decide which records a fold
//!   leaves out, so the defect lands as another multi-check row rather than an
//!   isolating one. It is a known gap, recorded here rather than closed:
//!   whoever closes it decides first whether a wider row buys more than it
//!   costs, the way `Defect::DedupIgnoresTheEntryType`'s four-check row does.

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
    MAX_AGGREGATION_BUCKETS, MetadataFilter, MeterTypeId, RecordOrigin, UsageRecord,
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
    /// Stamps its own insert time into `accepted_at`, discarding the instant
    /// it was handed — the mistake a backend makes by declaring the column
    /// `DEFAULT now()` and leaving it out of the insert, or by writing
    /// `now()` into it outright.
    ///
    /// DESIGN §3.1's "Server-assigned field fidelity" names this one
    /// outright. A plugin *"persists the values it was handed on the entry it
    /// stores, and every read path returns those. It does not re-derive,
    /// default, or refresh one — `accepted_at` in particular is not the
    /// store's own insert time."*
    ///
    /// It is applied on admission, so every read path answers the substituted
    /// instant and so does the absorbed retry. What that costs the matrix is
    /// stated where it is spent: see [`MUTANT_INSERT_INSTANT`].
    StampsItsOwnAcceptedAt,
    /// Writes `live` into `origin` whatever it was handed — the mistake a
    /// backend makes by declaring the column `DEFAULT 'live'` and leaving it
    /// out of the insert, or by hard-coding the live route in a port written
    /// before the backfill one existed.
    ///
    /// DESIGN §3.1's "Server-assigned field fidelity" names this one too: a
    /// plugin *"does not re-derive, default, or refresh one"*. `origin` is
    /// the field of the four a default is most natural on, because one of its
    /// two values is overwhelmingly the common case and a column that
    /// defaults to it looks right in every test a porter writes from live
    /// traffic.
    ///
    /// Applied on admission, like [`Self::StampsItsOwnAcceptedAt`] and for
    /// the same reason: a defaulted column is written once, on the way in,
    /// and every read path afterwards answers what was written.
    DefaultsOriginToLive,
    /// Selects on `window_start` instead of `window_end` — the mistake a
    /// backend makes by porting the pre-period point-in-time column.
    SelectsOnWindowStart,
    /// Keys dedup on `(tenant, type, idempotency_key, entry_type)`, omitting
    /// the two covered-period bounds — the derived identity with the period
    /// struck out.
    DedupIgnoresThePeriod,
    /// Keys dedup on `(tenant, type, idempotency_key, window_start,
    /// window_end)`, omitting `entry_type` — the derived identity with its
    /// sixth input struck out.
    ///
    /// DESIGN §3.3 names this one in the plugin obligation *"Enforce the
    /// six-part identity, `entry_type` included"*: *"A plugin that
    /// deduplicates on the other five components alone treats every
    /// invalidation as a collision with its target."* It is the mistake a
    /// backend makes by carrying the pre-invalidation unique constraint
    /// forward, or by naming five of the six columns in a conflict target.
    DedupIgnoresTheEntryType,
    /// Admits on all six identity inputs and **reads the colliding entry
    /// back** on five of them, `entry_type` struck out — so a retry of a
    /// withdrawn record is answered against the invalidation that withdrew
    /// it rather than against the record itself.
    ///
    /// DESIGN §3.3's obligation names three places `entry_type` has to
    /// appear and this is the second of them: *"Everything keyed on identity
    /// — a unique constraint or conflict target, the read-back of a
    /// conflicting entry, an in-batch dedup map — includes `entry_type` or
    /// keys on `id`, which covers all six inputs."* A backend can get the
    /// first right and the second wrong, and the mistake is an easy one: the
    /// `INSERT … ON CONFLICT (six columns) DO NOTHING` names all six, and the
    /// `SELECT` that follows it — which exists only because `DO NOTHING`
    /// returns no row to compare against — is written by hand against the
    /// idempotency key and its covered period, the shape the pre-invalidation
    /// model made natural.
    ///
    /// Such a read-back can see two rows, and this subject takes the later
    /// one. That is not an arbitrary tie-break: an invalidation names a
    /// target that must already be stored, so of any record and invalidation
    /// sharing those five components the invalidation is necessarily the
    /// later arrival. A backend reading back *the* entry on a key finds the
    /// withdrawal.
    ConflictReadBackIgnoresTheEntryType,
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
    /// Decides every collision by reading the ledger and then writes anyway:
    /// the dedup identity carries no unique constraint, so the outcome a
    /// caller reads is right and a second row lands beside the first.
    ///
    /// This is the mistake a backend makes by leaving the dedup logic in the
    /// application and the constraint out of the schema - the `SELECT` that
    /// decides absorb-or-conflict is written, the `INSERT` after it is
    /// unconditional, and nothing in the table refuses the duplicate. It is
    /// the defect DESIGN §3.1's "Dedup identity" row states the floor
    /// against: *"One identity yields at most one entry on every read path,
    /// fold, reconciliation figure, and materialised aggregate."*
    ///
    /// **A subject that answered differently instead would be the wrong
    /// shape.** Every collision outcome this subject returns is the
    /// conforming one, so it passes every assertion about an absorb, a
    /// conflict and an in-batch resolution; what it fails is the read-back.
    /// A subject wrong in its answers would fail those assertions and leave
    /// the floor itself untested, which is the half `dedup-floor` adds.
    ///
    /// **What the row this subject anchors does and does not establish**,
    /// measured by neutering each of `dedup-floor`'s nine assertions in turn
    /// rather than inferred. It reaches exactly the two assertions that read
    /// the ledger back: the row count on `list_usage_records`, and the
    /// `COUNT` fold over the same range. **Neither is individually
    /// necessary** — a duplicate row shows on both paths, so neutering either
    /// leaves the other reporting and the row unchanged; neutering both
    /// together takes `dedup-floor` out of this subject's row, and nothing
    /// else changes. Isolating one from the other needs a subject whose fold
    /// disagrees with its own ledger page: a `COUNT` served from a
    /// materialised aggregate refreshed once per submission would count a
    /// duplicate the ledger page also shows, and is the honest shape of that
    /// second defect.
    ///
    /// **`server-field-round-trip` passes this subject on two orderings
    /// rather than on anything structural**, and the next author to touch
    /// that check should know it. That check retries an entry, so this
    /// subject stores a duplicate of it carrying the retry's own
    /// `accepted_at` and `origin` — which the check asserts are *not* what a
    /// read answers with. It passes because the duplicate does not exist
    /// until the fourth of its five properties has run, so the three reads
    /// before that meet one row; and because the fifth looks its entry up by
    /// `id` through `get_usage_record`, which answers the first match while
    /// [`Ledger::records`] is in admission order. Measured, not reasoned: a
    /// row count added among the first four properties leaves this subject's
    /// row unchanged, and the same count added after the retry puts
    /// `server-field-round-trip` into it. A point read answering the later
    /// row would do the same.
    ///
    /// Six of `dedup-floor`'s nine assertions are reached by **no** subject
    /// in this module, each confirmed to execute and to be satisfied by the
    /// reference by inverting it: the retry, the divergent submission, both
    /// "the earlier entry of a batch is accepted" assertions, and the
    /// identical in-batch pair's absorb. The seventh,
    /// [`Defect::BatchResolvesAgainstThePreCallLedger`], closed the divergent
    /// in-batch conflict and is individually load-bearing.
    ///
    /// Of the six, two are **structurally** out of reach and one is nearly
    /// so. A subject refusing the first entry of an identity fails most of
    /// the suite and is a caricature rather than a mistake anyone makes,
    /// which accounts for both "earlier entry accepted" assertions. The
    /// identical in-batch absorb is the third: an absorb and a second
    /// acceptance both answer `Ok` carrying an entry equal in every
    /// caller-supplied field, so no outcome tells them apart and only the row
    /// count can — which is the floor half, not that assertion.
    ///
    /// **The retry and the divergent submission are a deliberate gap**,
    /// recorded here rather than closed. The subject that would close them is
    /// a backend absorbing a divergent re-delivery of a *record*: the
    /// withdrawal-only [`Defect::AbsorbsAWithdrawalWithAnotherReason`] never
    /// reaches one. It is writable against this SPI and it would isolate, but
    /// it is a **second** subject for the site
    /// [`Defect::ConflictReadBackIgnoresTheEntryType`] already covers — the
    /// read-back of a conflicting entry, the second of DESIGN §3.3's three —
    /// whereas the in-batch map that was built was a named site with no
    /// subject at all. Whoever closes it should weigh that first: a module of
    /// subjects is worth more per subject when each names a site nothing else
    /// names.
    LedgerHasNoUniqueConstraint,
    /// Resolves every entry of a batch against the ledger **as it stood
    /// before the call**, so two same-identity entries in one
    /// `create_usage_records` are both reported accepted instead of the later
    /// resolving against the earlier.
    ///
    /// **The third of DESIGN §3.3's three identity sites**, and the one that
    /// had no subject at all until this one: see this module's header for the
    /// obligation and for which subject covers which site.
    /// [`Self::DedupIgnoresTheEntryType`] and
    /// [`Self::ConflictReadBackIgnoresTheEntryType`] take the first two, and
    /// neither touches a batch.
    ///
    /// Strictly it is the site **missing** rather than mis-keyed, which is
    /// the stronger form of the same mistake and the one the SPI states
    /// outright in
    /// [`create_usage_records`](crate::plugin_api::UsageCollectorPluginV1::create_usage_records):
    /// *"Two same-identity entries in one call resolve later against earlier:
    /// the later is absorbed when its caller-supplied fields equal the
    /// earlier accepted entry's, and conflicts otherwise."* A backend that
    /// reads once, decides every row against that read, and then writes them
    /// all makes exactly this mistake.
    ///
    /// **The write still dedups**, and that is what keeps this subject wrong
    /// in one place rather than two. The unique constraint is DESIGN's first
    /// site and [`Self::LedgerHasNoUniqueConstraint`] already strikes it out;
    /// a subject missing both would be wrong in two of the three named places
    /// and could isolate neither. So the duplicate row is refused by the
    /// ledger and silently dropped — which is what `ON CONFLICT … DO NOTHING`
    /// does — and the damage is confined to the outcome the caller is handed:
    /// an acceptance reported for a row that was never written.
    BatchResolvesAgainstThePreCallLedger,
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
        | Defect::StampsItsOwnAcceptedAt
        | Defect::DefaultsOriginToLive
        | Defect::DedupIgnoresThePeriod
        | Defect::DedupIgnoresTheEntryType
        | Defect::ConflictReadBackIgnoresTheEntryType
        | Defect::IgnoresScopeOnThePointRead
        | Defect::AbsorbsAWithdrawalWithAnotherReason
        | Defect::RefusesAWithdrawalWithTheSameReason => Box::new(WrappedReference::new(defect)),
        Defect::SelectsOnWindowStart
        | Defect::FoldsTheInvalidation
        | Defect::LedgerHasNoUniqueConstraint
        | Defect::BatchResolvesAgainstThePreCallLedger => Box::new(MutantLedger::new(defect)),
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
/// One qualification, and it holds for all nine wrapped defects:
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
    /// entry that claimed it. Unused by the other eight defects.
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
    /// The entry-type-blind dedup index [`Defect::DedupIgnoresTheEntryType`]
    /// keys on: `(tenant_id, gts_type_id, idempotency_key, window_start,
    /// window_end)` to the entry that claimed it. Unused by the other eight
    /// defects.
    ///
    /// A claim is recorded when the entry is admitted rather than after the
    /// inner backend stores it, which is a unique index written inside the
    /// same transaction, and it therefore leaves a claim behind for an entry
    /// the inner backend then refuses. That case is unreachable here, and
    /// the reason is an invariant of this index rather than a fact about any
    /// one check. An entry that gets past this index either found no claim
    /// on its five components or found one carrying its own derived `id`, so
    /// every entry the inner backend ever stored under a given five-tuple
    /// carries the one `id` that tuple's claim names. The inner backend
    /// refuses only a divergent retry of a stored `id`, so an entry it
    /// refuses had to pass a claim of that same `id` on the way in - and the
    /// claim left behind names that same identity, which is the only thing
    /// this index is ever asked about.
    entry_type_blind_keys: Mutex<BTreeMap<EntryTypeBlindKey, UsageRecord>>,
    /// The rows [`Defect::ConflictReadBackIgnoresTheEntryType`] reads a
    /// colliding entry back from: the same five components, to **every**
    /// entry accepted under them, in arrival order. Unused by the other eight
    /// defects.
    ///
    /// A `Vec` rather than one entry, because two rows under one five-tuple
    /// is the whole situation the defect is about, and the defect is which
    /// of them the read-back picks. It takes the last, and that is a
    /// structural choice rather than a tie-break: an invalidation names a
    /// target that must already be stored, so of a record and an
    /// invalidation sharing five components the invalidation is always the
    /// later arrival.
    ///
    /// Written after the inner backend accepts rather than before, which is
    /// the opposite of the two indexes above and is what keeps this subject
    /// wrong in one way only. This index decides nothing about admission: an
    /// entry is remembered here exactly when the exemplar stored it, so the
    /// mirror cannot drift and no entry the inner refused is ever read back
    /// from it.
    five_component_rows: Mutex<BTreeMap<EntryTypeBlindKey, Vec<UsageRecord>>>,
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

/// The five inputs an entry-type-blind dedup identity keys on:
/// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`.
///
/// Named for what it leaves out. The derived identity reads six inputs, and
/// the one missing here is `entry_type` — because a record and its
/// invalidation agree on the other five. Two defects key on it, each
/// striking that input out of a different one of the three places DESIGN
/// §3.3 names: [`Defect::DedupIgnoresTheEntryType`] out of the unique
/// constraint, and [`Defect::ConflictReadBackIgnoresTheEntryType`] out of
/// the read-back of a conflicting entry.
type EntryTypeBlindKey = (
    Uuid,
    String,
    String,
    time::OffsetDateTime,
    time::OffsetDateTime,
);

impl WrappedReference {
    /// Wraps a fresh reference backend.
    fn new(defect: Defect) -> Self {
        Self {
            inner: InMemoryReferencePlugin::new(),
            defect,
            period_blind_keys: Mutex::new(BTreeMap::new()),
            entry_type_blind_keys: Mutex::new(BTreeMap::new()),
            five_component_rows: Mutex::new(BTreeMap::new()),
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
            // A `DEFAULT now()` column the insert never names: the instant
            // the gateway stamped is discarded and the store's own takes its
            // place.
            Defect::StampsItsOwnAcceptedAt => Ok(UsageRecord {
                accepted_at: MUTANT_INSERT_INSTANT,
                ..record
            }),
            // A `DEFAULT 'live'` column the insert never names: whichever
            // route the gateway stamped, the row records the common one.
            Defect::DefaultsOriginToLive => Ok(UsageRecord {
                origin: RecordOrigin::Live,
                ..record
            }),
            Defect::DedupIgnoresThePeriod => self.claim_period_blind_key(record),
            Defect::DedupIgnoresTheEntryType => self.claim_entry_type_blind_key(record),
            // Enumerated rather than caught by a wildcard. A new defect
            // routed here and forgotten would otherwise pass its
            // entries through untouched and report no violation at all;
            // spelling the variants out makes that a compile error instead
            // of a matrix row whose subject does nothing. The point read's
            // defect is applied on the read path, the two withdrawal defects
            // and the conflict read-back around the inner call, and the last
            // two never reach this type at all — `mutant` routes them to
            // `MutantLedger` — but exhaustiveness is the whole point.
            Defect::IgnoresScopeOnThePointRead
            | Defect::AbsorbsAWithdrawalWithAnotherReason
            | Defect::RefusesAWithdrawalWithTheSameReason
            | Defect::ConflictReadBackIgnoresTheEntryType
            | Defect::SelectsOnWindowStart
            | Defect::FoldsTheInvalidation
            | Defect::LedgerHasNoUniqueConstraint
            | Defect::BatchResolvesAgainstThePreCallLedger => Ok(record),
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

    /// Admits an entry only if no other entry already holds its
    /// `(tenant, type, idempotency_key, window_start, window_end)`.
    ///
    /// This is a unique index over the derived identity with `entry_type`
    /// struck out, and that omission is the whole of the defect. An
    /// invalidation repeats every caller-supplied field of its target but
    /// the reason code, its idempotency key and covered period included, so
    /// this index sees the record's own five components arrive a second time
    /// and answers the collision DESIGN names: *"A plugin that deduplicates
    /// on the other five components alone treats every invalidation as a
    /// collision with its target."*
    ///
    /// The covered period is in the index for the same reason the entry type
    /// is in [`Self::claim_period_blind_key`]'s: one key over two periods is
    /// two entries, and an index blind to the bounds as well would be wrong
    /// in a second way, in a subject that must be wrong in exactly one.
    ///
    /// A resubmission of an entry that already claimed the key carries the
    /// same derived `id`, so an idempotent replay still reaches the inner
    /// backend and is answered by it.
    fn claim_entry_type_blind_key(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let key = (
            record.tenant_id,
            record.gts_type_id.as_str().to_owned(),
            record.idempotency_key.as_str().to_owned(),
            record.window_start,
            record.window_end,
        );
        let mut claimed = self.entry_type_blind_keys.lock().map_err(|_| {
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

    /// What the two post-inner defects do to one entry's outcome, and the
    /// place [`Defect::ConflictReadBackIgnoresTheEntryType`] keeps its
    /// mirror of the ledger up to date.
    ///
    /// * [`Defect::AbsorbsAWithdrawalWithAnotherReason`]: a conflict on a
    ///   withdrawal is answered as an absorb of the stored entry.
    /// * [`Defect::ConflictReadBackIgnoresTheEntryType`]: an outcome the
    ///   inner backend decided against a *stored* entry is decided again
    ///   here, against whichever entry a five-component read-back finds.
    ///
    /// Both leave admission alone, which is why they sit here rather than in
    /// [`Self::on_admission`].
    fn after_admission(
        &self,
        record: &UsageRecord,
        outcome: Result<UsageRecord, UsageCollectorPluginError>,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if self.defect == Defect::ConflictReadBackIgnoresTheEntryType {
            return self.read_the_conflict_back_on_five_components(record, outcome);
        }
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

    /// [`Defect::ConflictReadBackIgnoresTheEntryType`]: the collision branch,
    /// decided against a row found on five components.
    ///
    /// The inner backend has already decided admission on all six identity
    /// inputs, so this reaches an entry whose `id` is already stored exactly
    /// when the insert was a no-op — which is the only case a real backend's
    /// read-back runs in, since `ON CONFLICT … DO NOTHING` returns no row to
    /// compare against. A fresh entry is passed through untouched and
    /// remembered.
    ///
    /// The read-back takes the last row under the five components, and the
    /// comparison against it is the ordinary one
    /// ([`UsageRecord::caller_supplied_eq`]): equal is an absorb answering
    /// with that row, different is `IdempotencyConflict` carrying it. Only
    /// *which row* is wrong here, which is the whole of the defect.
    fn read_the_conflict_back_on_five_components(
        &self,
        record: &UsageRecord,
        outcome: Result<UsageRecord, UsageCollectorPluginError>,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let key = five_component_key(record);
        let mut rows = self.five_component_rows.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's ledger mirror lock is poisoned")
        })?;
        let stored = rows.entry(key).or_default();
        if !stored.iter().any(|entry| entry.id == record.id) {
            if outcome.is_ok() {
                stored.push(record.clone());
            }
            return outcome;
        }
        // Unreachable: the branch above returned unless some row under this
        // key carries the submitted `id`, so the list is not empty here. A
        // fall-through rather than an `expect`, because a subject the
        // discrimination matrix drives should report the inner backend's own
        // answer if this ever stops holding, not abort the run.
        let Some(found) = stored.last().cloned() else {
            return outcome;
        };
        drop(rows);
        if found.caller_supplied_eq(record) {
            return Ok(found);
        }
        Err(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            found,
        ))
    }
}

/// The five caller-supplied identity components of `record`, `entry_type`
/// struck out.
fn five_component_key(record: &UsageRecord) -> EntryTypeBlindKey {
    (
        record.tenant_id,
        record.gts_type_id.as_str().to_owned(),
        record.idempotency_key.as_str().to_owned(),
        record.window_start,
        record.window_end,
    )
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
        // reporting the per-entry refusals it already has.
        //
        // **Reached by exactly one subject today, `DedupIgnoresTheEntryType`,
        // and that count is part of the claim.** The suite sends three
        // batches, and only one of them can empty this list.
        //
        // `at-most-one-invalidation`'s is two withdrawals of a target already
        // stored. Both repeat that target's idempotency key over its covered
        // period, so the entry-type-blind index refuses each of them against
        // the claim the target left and no survivor is handed on. Of the
        // other two defects that can refuse an entry here,
        // `DedupIgnoresThePeriod` admits the second withdrawal under the
        // claim the first left, both deriving one `id`, and
        // `RefusesAWithdrawalWithTheSameReason` finds no withdrawal stored to
        // refuse either against.
        //
        // `dedup-floor`'s two are pairs of **records** sharing all six
        // identity inputs, so each pair derives one `id` and neither index
        // above ever holds a claim under a different one - the second entry
        // of each pair passes on the claim the first left. That check submits
        // no invalidation at all, so the third defect never fires either.
        // Both pairs therefore reach the inner backend whole, which is the
        // point: the in-call resolution stays the exemplar's.
        //
        // A later defect that reaches this branch a second way must say so
        // here rather than inherit a paragraph written about one subject: the
        // reasoning above is per subject, not a general argument. It was
        // established by instrumenting this line rather than by reading it,
        // and re-established the same way when `dedup-floor` added its two
        // batches.
        //
        // So the guard is a live path rather than a precaution, which is also
        // why it is repeated here rather than left to the inner backend to
        // raise.
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

/// The instant [`Defect::StampsItsOwnAcceptedAt`] writes in place of the one
/// it was handed.
///
/// **A fixed instant of this subject's own, never `OffsetDateTime::now_utc()`
/// — which is what the `DEFAULT now()` column it models would really write.**
/// A subject whose answer moves between runs makes a failure unreproducible,
/// and nothing here needs to be "now" in order to be wrong: the check
/// compares what a read answered against what it submitted, never against
/// this value.
///
/// Later than every acceptance instant the suite submits, which is the
/// direction a real insert time lies in. `CONTRACT_ACCEPTED_AT` is the UNIX
/// epoch plus `20_454` days and `server-field-round-trip` offsets its own
/// four from that by hours; this is `21_000` days, clear of all of them, and
/// that distinctness is what makes the substitution observable at all.
///
/// **This subject reaches every assertion `server-field-round-trip` makes**,
/// which bounds what the matrix row proves. Admission is upstream of all four
/// read paths and of the absorb, so each of them answers the substituted
/// instant and each reports on its own — and no single one of them is
/// *necessary* for the row, because the other four still report without it.
/// The row establishes that the check as a whole notices a backend stamping
/// its own instant, not that any one path's assertion is load-bearing.
/// [`Defect::DefaultsOriginToLive`] is the same shape and carries the same
/// caveat.
///
/// It reaches `accepted_at` alone, and which of the check's four fields has a
/// subject at all is stated once in this module's header rather than on each
/// defect: see "The four server-assigned fields, and which two have a
/// subject".
const MUTANT_INSERT_INSTANT: time::OffsetDateTime =
    time::OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::days(21_000));

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
/// exclusion, the ungrouped `SUM` the fold check reads, which rows the three
/// scope-carrying read paths answer with, since `server-field-round-trip`
/// landed that the feed **delivers** the entries of a subscribed meter at
/// all, and — since `dedup-floor` landed — the ungrouped `COUNT`, which is
/// the only fold shape besides `SUM` that `run_all` dispatches at
/// `DedupLevel::Linearizable` (`at-most-one-invalidation` sends one too, but
/// only under an `Eventual` declaration, which the matrix does not run).
/// Each was measured by breaking it and watching a row grow, not inferred
/// from the check list: the `COUNT` arm of [`fold_value`] was measured by
/// returning `count + 1`, which took both subjects built on this type out of
/// their own rows.
///
/// Everything else here is level with the reference and **unpinned**: the
/// ledger page's *order* (its membership is pinned, its sort is asserted by
/// no check), the grouped folds and the three that read a quantity,
/// the reconciliation figures,
/// and everything about the feed page but its delivering entries — its seek,
/// its scanned-entry cursor rule, its subscription and scope gates, and its
/// retention refusal. Those four were measured too, each by breaking it and
/// watching the matrix stay green: a feed that advances its cursor only past
/// the rows it admits, one that answers every tenant's entries under any
/// grant, one that answers every meter's, and one that skips a count of rows
/// instead of seeking all pass every check the suite runs today. They are
/// mirrored because the checks that read them are coming, and each becomes
/// pinned by the check that first dispatches it. An unpinned behaviour is
/// where this mirror can still rot in silence, which is the argument for
/// keeping it level now rather than letting it answer `Internal` until
/// someone needs it.
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

    /// Decides one entry and writes it, under this subject's own admission
    /// rule.
    ///
    /// Every subject but [`Defect::LedgerHasNoUniqueConstraint`] admits
    /// through [`admit`], which is the reference's decision: a collision on
    /// `id` resolves by caller-supplied fields and only a fresh entry is
    /// written. That one admits through
    /// [`admit_without_a_unique_constraint`], which decides the same way and
    /// writes regardless.
    ///
    /// **This is why that defect carries a ledger of its own rather than
    /// wrapping the reference.** The other two here change a predicate the
    /// inner backend owns; this one changes what the inner backend *stores*,
    /// and a wrapper cannot make [`InMemoryReferencePlugin`] hold a second
    /// row under one `id` - its own admission refuses to. A wrapper keeping
    /// the duplicates in a side ledger would then have to re-implement every
    /// read path's selection, ordering and paging to merge them back in,
    /// which is this type with extra steps.
    fn admit_here(
        &self,
        ledger: &mut Ledger,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if self.defect == Defect::LedgerHasNoUniqueConstraint {
            return admit_without_a_unique_constraint(ledger, record);
        }
        admit(ledger, record)
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
        self.admit_here(&mut ledger, record)
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
        if self.defect == Defect::BatchResolvesAgainstThePreCallLedger {
            return Ok(resolve_against_the_pre_call_ledger(&mut ledger, records));
        }
        Ok(records
            .into_iter()
            .map(|record| self.admit_here(&mut ledger, record))
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
    decide_against(ledger.records(), record)
}

/// The same decision over an arbitrary set of rows.
///
/// Split out for [`resolve_against_the_pre_call_ledger`], which decides a
/// whole batch against a snapshot rather than against the live ledger. The
/// decision itself stays in one function: a second copy of it is how a mirror
/// of the exemplar starts disagreeing with the exemplar in a place no defect
/// names.
fn decide_against<'a>(
    rows: impl IntoIterator<Item = &'a UsageRecord>,
    record: &UsageRecord,
) -> Result<Option<UsageRecord>, UsageCollectorPluginError> {
    match rows.into_iter().find(|entry| entry.id == record.id) {
        Some(stored) if stored.caller_supplied_eq(record) => Ok(Some(stored.clone())),
        Some(stored) => Err(UsageCollectorPluginError::idempotency_conflict(
            record.idempotency_key.as_str(),
            stored.clone(),
        )),
        None => Ok(None),
    }
}

/// [`Defect::BatchResolvesAgainstThePreCallLedger`]: every entry of one batch
/// decided against the rows the ledger held **before** the call.
///
/// The snapshot is taken once, up front, and no entry admitted by this call
/// is added to it. That is the whole of the defect: a backend that reads its
/// dedup state once for the batch has no in-batch dedup map, so the second
/// entry of a same-identity pair inside the call is decided as though the
/// first had never arrived.
///
/// **The write is the ordinary one.** [`admit`] still refuses a duplicate
/// `id`, because the unique constraint is DESIGN §3.3's *first* named site
/// and [`Defect::LedgerHasNoUniqueConstraint`] is the subject that strikes
/// that one out. So the row the constraint refuses is dropped without a
/// word — an `ON CONFLICT … DO NOTHING` — and the outcome the caller reads
/// comes from the snapshot instead. The caller is told a row was accepted
/// that the store never wrote, which is the shape of this mistake in a
/// backend that is otherwise conforming.
///
/// A same-identity pair that is **identical** is indistinguishable here from
/// a conforming absorb, and necessarily so: both answer `Ok` carrying an
/// entry equal in every caller-supplied field, and only the row count could
/// tell them apart — which the surviving constraint keeps at one. The
/// divergent pair is where this subject shows.
fn resolve_against_the_pre_call_ledger(
    ledger: &mut Ledger,
    records: Vec<UsageRecord>,
) -> Vec<Result<UsageRecord, UsageCollectorPluginError>> {
    let before: Vec<UsageRecord> = ledger.records().cloned().collect();
    let mut outcomes = Vec::with_capacity(records.len());
    for record in records {
        let answer = match decide_against(before.iter(), &record) {
            Ok(Some(stored)) => Ok(stored),
            Ok(None) => Ok(record.clone()),
            Err(err) => Err(err),
        };
        let _written = admit(ledger, record);
        outcomes.push(answer);
    }
    outcomes
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

/// [`Defect::LedgerHasNoUniqueConstraint`]: decides one entry against the
/// ledger exactly as [`admit`] decides it, and writes it whatever the
/// decision was.
///
/// The answer is the conforming one in all three cases - the stored entry for
/// an exact retry, `IdempotencyConflict` carrying it for a divergent
/// submission, the entry itself for a fresh one - because it is
/// [`decide`]'s, the same function [`admit`] consults. Only the write differs,
/// and it differs by not consulting anything: the schema this models has no
/// unique constraint on the dedup identity, so the insert after the lookup
/// cannot be refused.
///
/// [`decide`] finds a row by `id` with `Iterator::find`, so it goes on
/// answering with the **first** row written under an identity however many
/// duplicates pile up behind it. That is what keeps this subject wrong in one
/// way only: a caller reads the same outcomes a conforming backend would give,
/// and the ledger holds rows a conforming backend would not.
fn admit_without_a_unique_constraint(
    ledger: &mut Ledger,
    record: UsageRecord,
) -> Result<UsageRecord, UsageCollectorPluginError> {
    let answer = match decide(ledger, &record) {
        Ok(Some(stored)) => Ok(stored),
        Ok(None) => Ok(record.clone()),
        Err(err) => Err(err),
    };
    ledger.push(record);
    answer
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
