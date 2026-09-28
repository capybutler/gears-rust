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
//! *Behaviourally* is the exact word. Some subjects wrap a real reference
//! backend and are that backend plus one interception; the rest
//! re-implement it, and a re-implementation is the same backend only as far
//! as the checks can see. [`carries_its_own_ledger`] is which is which, and
//! [`MutantLedger`] states how far that is.
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
//! strongest form the subject can take. The defects that fit are quantity,
//! the store's own acceptance instant, a defaulted origin, period-blind
//! dedup, entry-type-blind dedup, the entry-type-blind conflict read-back,
//! point-read scope, the undecided converged-only lookup, the three
//! withdrawal defects, the three bootstrap defects, the two
//! position-encoding defects and the three empty-selection defects: three
//! rewrite a field on the way in, three keep an index beside the ledger —
//! two to refuse an admission, one only to decide which stored entry a
//! collision is answered with — two change what the point read answers, one
//! by substituting the scope and one by declining to decide, three rewrite
//! how a second withdrawal of a record is answered — one refusing it, one
//! absorbing it and one answering it against the record it withdraws —
//! three reinterpret what `FeedStart::Oldest` names, two re-encode the
//! position a page hands back, and three rewrite what a fold answers where
//! nothing survived.
//!
//! **A ledger of its own** ([`MutantLedger`]) is needed by the rest
//! (selection column, fold exclusion, missing unique constraint,
//! missing in-batch dedup map, a divergent write that displaces the
//! survivor, the three `LATEST` orders, the five feed defects, and the four
//! retention-refusal defects), because each changes something the inner
//! backend owns and no interception can reach it: which column a range
//! meets, which rows a fold walks, what admission writes, what a batch's own
//! rows are decided against, which row a decided collision leaves behind,
//! which row a fold ranks highest, in what order a feed page walks, which
//! entries its cursor moves past, where a page resumes, whether a bounded
//! replay says it is finished, and which question the retention refusal asks
//! of its own marks. It mirrors the reference where the defect is not, and it
//! is smaller in one stated way that no check reaches — see
//! [`MutantLedger`].
//!
//! # The feed defects: inside the page loop, guarding it, and before it
//!
//! `read_feed_page` is one loop over a ledger, and it makes four decisions a
//! defect can land in: what **order** it walks the ledger in, where the page
//! **resumes**, how far the cursor **advances**, and whether the page
//! **closes**. There is a subject for each, and two for the third, because a
//! cursor can stop short of where it belongs or run past it. All five are
//! routed to [`MutantLedger`]:
//!
//! * **What order it walks in** —
//!   [`Defect::FeedOrdersByTheAcceptanceInstant`], the one ordering §3.1
//!   says is not claimed.
//! * **Where it resumes** — [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`],
//!   the keyset off-by-one.
//! * **How far the cursor advances** —
//!   [`Defect::FeedCursorCountsAdmittedEntries`], the scope and
//!   subscription gate moved above the cursor, which stops it short; and
//!   [`Defect::AFeedPageDropsTheEntryAtItsLimit`], the limit checked after
//!   the cursor has moved, which runs it one entry past.
//! * **Whether it closes** — [`Defect::ABoundedReplayNeverCloses`], the
//!   second half of [`FeedPage::next`]'s two-element enumeration.
//!
//! Four of the five are one line. The order is the exception, because an
//! order is not a line: that subject sorts the ledger by the key it orders
//! on and reads positions off that key, which is what a backend with no
//! feed-order column of its own actually does.
//!
//! **A fifth decision is made before any of those four**, and it has four
//! subjects of its own: whether the page is served at all. DESIGN §3.3's
//! `feed-retention-refusal` row is the rule, the refusal reads what a sweep
//! removed and nothing else, and the row names three things that are not
//! inputs to it. [`Defect::ServesAShortPageWhereRetentionTruncatedACursor`]
//! asks nothing, [`Defect::TheRetentionRefusalReadsTheCallersGrant`] asks
//! what the caller's grant would have carried,
//! [`Defect::RefusesEveryCursorOnceASweepHasRun`] asks only whether a sweep
//! ran, and [`Defect::TheRetentionRefusalIgnoresTheSubscription`] asks it of
//! every type at once. All four show nothing until a sweep has run, so
//! [`mutant`] hands them out like any other subject and [`super::run_all`]
//! finds them conforming; they are reached through
//! [`drivable_ledger_mutant`] and [`super::run_all_with_retention`], and
//! `contract_tests`' `RETENTION_DRIVEN_MATRIX` is where their columns are
//! asserted.
//!
//! None of them is a wrapper, and none could be: the loop and the
//! refusal above it are the method, so an interception would have had to
//! re-implement it anyway — and a wrapper delegating to a conforming backend
//! cannot make that backend fail to refuse.
//!
//! **Three more land above the loop, and all three are wrappers.** Before
//! the loop runs there is one further decision: what `start` *names*. A
//! defect there substitutes a different start or declines the call, and
//! everything after it is the exemplar's own — which is what an
//! interception is for. DESIGN §3.3's `feed-bootstrap-position` row is the
//! rule all three break, and it has three clauses:
//!
//! * **Never the head** — [`Defect::AFeedBootstrapReadStartsAtTheHead`],
//!   an absent cursor read as "start from now".
//! * **Never refused on the retention floor** —
//!   [`Defect::RefusesTheOldestStartAfterASweep`], the cursor refusal
//!   decided above the branch instead of inside it.
//! * **At the oldest entry the subscription retains** —
//!   [`Defect::SkipsTheOldestEntryASweepLeft`], the bootstrap position
//!   derived from the retention mark with the boundary the wrong side of
//!   it.
//!
//! The last two show nothing until a sweep has run, so
//! [`mutant`] hands them out like any other subject and
//! [`super::run_all`] finds them conforming. They are reached through
//! [`drivable_mutant`] and [`super::run_all_with_retention`] instead, and
//! `contract_tests` says why neither has a row in the discrimination
//! matrix.
//!
//! # The two position defects, and why they are wrappers too
//!
//! A sixth decision is made **after** the loop: what the page's
//! continuation is *encoded as*. DESIGN §3.1 leaves a position's structure
//! to the plugin and takes its **encoded size** back — the size *"may not
//! grow with a subscription's breadth"* — so a defect here is a subject that
//! delegates the whole read and re-encodes what came back.
//! [`Defect::AFeedPositionIsKeyedPerTenant`] appends one component per
//! tenant the subscribed types hold entries under, and
//! [`Defect::AFeedPositionIsKeyedPerSubscribedType`] one per type named.
//! Both strip their own components off again on the way in, so a position
//! either of them issued resumes exactly where the exemplar's would: what
//! they get wrong is the size and nothing else, which is what
//! `feed-position-bounded` measures and the only thing it measures.
//!
//! **Neither could be a `MutantLedger`**, and the reason is the mirror of
//! the feed defects above: those are the page loop and the refusal guarding
//! it, which a wrapper would have to re-implement, while these two are a
//! pure function of the position the loop produced. A ledger of their own
//! would be the SPI's seven methods re-implemented to change the last line
//! of one.
//!
//! # DESIGN's three empty-selection clauses, and the subject for each
//!
//! §3.3's plugin obligations state what a fold answers where nothing
//! survived, and they state it as three clauses rather than one rule:
//! *"`SUM` and `COUNT` are defined over an empty selection and report `0`;
//! `MAX`, `MIN` and `LATEST` are not and report absent. […] A grouped query
//! yields no bucket for a group nothing survives in."* Each of the three has
//! one subject, and each subject is a wrapper, because all three are
//! functions of the answer the exemplar already computed:
//!
//! * **`SUM` and `COUNT` report `0`** — [`Defect::EmptySumIsAbsent`], which
//!   answers absent instead, as `SUM(x)` over no rows does in SQL.
//! * **`MAX`, `MIN` and `LATEST` report absent** —
//!   [`Defect::CoalescesEveryEmptyFoldToZero`], which answers `0` instead:
//!   the blanket `COALESCE` a porter reaches for once the clause above has
//!   bitten.
//! * **A grouped query yields no bucket** —
//!   [`Defect::AGroupNothingSurvivesInStillGetsABucket`], which yields one.
//!
//! The first two are each other's mirror on purpose. The obligation is a
//! **split**, so a check asserting one side alone is passed by the backend
//! that collapses it the other way, and one subject collapses it each way.
//! The third runs opposite to both: a grouped query answers *fewer* buckets
//! where an ungrouped one answers the same bucket emptied, so a backend with
//! one rule for "nothing survived" fails one of the two whichever rule it
//! holds.
//!
//! **`COUNT` over an empty selection has no subject of its own, and the gap
//! is recorded rather than closed.** `EmptySumIsAbsent` breaks the clause
//! the two folds share, so `invalidation-excluded-from-fold`'s `COUNT`
//! assertion is reached by no subject: neutering it changes no row of the
//! matrix. A second subject would be one for a site that already has one,
//! which is the reasoning [`Defect::LedgerHasNoUniqueConstraint`]'s own gap
//! is left open on. It would not be an absurd subject — a `COUNT` served
//! from a pre-aggregated per-bucket count is `SUM(c)`, which *is* `NULL`
//! over no rows, and the `TimescaleDB` plugin's rollup carries a `COALESCE`
//! for exactly that reason — so whoever closes it is deciding that a second
//! subject on one clause buys more than it costs.
//!
//! # DESIGN's three `LATEST` keys, and the subject for each
//!
//! §3.1 states the order as *"Greatest `window_end`, then greatest
//! `accepted_at`, then greatest `id` in byte order"*, and each of the three
//! is struck out by one subject: [`Defect::LatestIgnoresThePeriodEnd`],
//! [`Defect::LatestSkipsTheAcceptanceInstant`] and
//! [`Defect::LatestStopsAtTheAcceptanceInstant`]. The set is complete
//! because the order is: a fourth way to get it wrong would be a fourth key,
//! and there is no fourth key. [`LatestOrder`] is where that shows in the
//! code — one enum with one variant per key omitted, rather than three
//! branches scattered over a fold.
//!
//! One of the three is not new wrongness but recovered wrongness:
//! `LatestSkipsTheAcceptanceInstant` is the reference backend's own fold as
//! it stood before it was corrected. A subject that was once the exemplar is
//! the strongest evidence a defect is plausible.
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
//!   costs, the way `Defect::DedupIgnoresTheEntryType`'s wide row does.

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
use super::retention::ContractRetention;
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
    /// Answers absent for `SUM` over an empty selection, where DESIGN
    /// defines it as `0` — the reference backend's own fold before slice 1b
    /// corrected it.
    ///
    /// It is also `SUM`'s own answer in SQL: `SUM(x)` over no rows is
    /// `NULL`, so a backend gets this wrong by writing the obvious
    /// expression and nothing else. That makes it the one defect in this
    /// module a porter reaches without making a mistake, which is why the
    /// subject exists even though the rule it breaks is a single clause.
    EmptySumIsAbsent,
    /// Answers `0` for `MAX`, `MIN` and `LATEST` over an empty selection,
    /// where DESIGN has all three report absent — the mistake a backend
    /// makes by wrapping every fold in `COALESCE(…, 0)` once it has found
    /// out that `SUM` needs one.
    ///
    /// It is [`Self::EmptySumIsAbsent`]'s mirror, and the pair is why the
    /// empty-selection rule needs two subjects rather than one. DESIGN
    /// §3.3's obligation is a **split** — *"`SUM` and `COUNT` are defined
    /// over an empty selection and report `0`; `MAX`, `MIN` and `LATEST` are
    /// not and report absent"* — and a backend that collapses it answers
    /// one family with the other's answer. One subject collapses it each
    /// way, so a check asserting only one side would pass one of them.
    CoalescesEveryEmptyFoldToZero,
    /// Emits a bucket for a group nothing survives in, keyed from the rows
    /// the range selects rather than from the rows that survive the fold —
    /// the mistake a backend makes by leaving the withdrawal exclusion out
    /// of its `WHERE` and putting it in a `FILTER (WHERE …)` on the
    /// aggregate instead.
    ///
    /// That rewrite is exact under every fold **but** for which groups
    /// exist: `GROUP BY` then forms a group per key the range holds, and
    /// the group whose every row the filter removes comes back as a bucket
    /// carrying the empty fold's answer rather than not coming back at all.
    /// DESIGN §3.3's obligation ends on that case — *"A grouped query yields
    /// no bucket for a group nothing survives in"* — and it is the half of
    /// the empty-selection rule that runs the opposite way from the
    /// ungrouped one, which is why neither [`Self::EmptySumIsAbsent`] nor
    /// [`Self::CoalescesEveryEmptyFoldToZero`] reaches it.
    AGroupNothingSurvivesInStillGetsABucket,
    /// Absorbs a second withdrawal of a record even under another reason code —
    /// the mistake a backend makes by comparing only the dedup identity.
    AbsorbsAWithdrawalWithAnotherReason,
    /// Refuses a second withdrawal of a record even under the same reason code —
    /// the mistake a backend makes by keeping an at-most-one rule of its own.
    RefusesAWithdrawalWithTheSameReason,
    /// Conflicts a second withdrawal against the record it withdraws rather
    /// than against the accepted withdrawal.
    ///
    /// The mistake a backend makes by reading "at most one invalidation" as
    /// a rule about the **record**: the collision is detected on the right
    /// identity, and the entry handed back is then looked up through
    /// `invalidates` rather than taken from the identity that collided.
    /// DESIGN §3.3 rules that answer out in so many words — the `existing`
    /// is *"that invalidation, never the record"* — and a caller told its
    /// withdrawal collided with the measurement learns nothing it can act
    /// on: the gateway lifts an invalidation's conflict to
    /// `AlreadyInvalidated` naming `existing.id` and its reason code, and
    /// the record carries no reason code at all.
    ///
    /// **Wrong about the answer alone**, and only on a refusal raised for an
    /// entry that carries an invalidation. Admission is the exemplar's, so
    /// the ledger holds one withdrawal per record and every read, fold and
    /// feed page shows what a conforming backend's would; only the
    /// `existing` a refusal carries is substituted. That is why the subject
    /// is a wrapper, and the second confinement is what keeps it to one
    /// check even though two others do compare a conflict's `existing`:
    /// `dedup-floor` compares it twice and submits no invalidation at all,
    /// and `record-and-invalidation-distinct-identity` submits one and has
    /// all three of its submissions accepted, so neither raises a refusal
    /// this subject can rewrite.
    ///
    /// **No claim is made here about the gear's own history, and one that
    /// reads well is false.** Before `entry_type` became the sixth identity
    /// input, the derivation read five and told a measurement from its
    /// withdrawal by a reserved `inv:` key prefix instead — so a record and
    /// its withdrawal derived *different* ids then too, and this answer was
    /// never anything the gear gave. The subject exists because the clause
    /// is DESIGN's, not because the code once broke it.
    ConflictNamesTheRecord,
    /// Honours the scope on the list and aggregate paths and ignores it on
    /// the point read.
    IgnoresScopeOnThePointRead,
    /// Answers [`UsageCollectorPluginError::UsageRecordNotConverged`] for an
    /// entry it has already acknowledged, whenever the lookup asks for a
    /// converged one — a store whose point read goes to a replica it has not
    /// waited for.
    ///
    /// DESIGN §3.3's "Decide converged-only lookups" obligation names the
    /// failure: a lookup with `converged_only` *"returns the survivor once
    /// converged, never reports an acknowledged, retained entry missing, and
    /// answers `UsageRecordNotConverged` only until it can decide"*. A
    /// perpetual undecided answer is the second way to fail a caller that
    /// holds an acknowledgement, and the less visible one — reporting the
    /// entry missing at least ends the exchange, while this answer tells the
    /// caller to come back, and the gateway lifts the variant to a
    /// *retryable* conflict, so it does.
    ///
    /// It is the mistake a backend makes by reading `converged_only` as "say
    /// so when you are not sure" rather than as a question it is obliged to
    /// answer: a port that routes point reads to a replica pool, has no way
    /// to establish that the pool has caught up, and declines to decide
    /// rather than declining to lag.
    ///
    /// **Applied only where the inner backend answered `Ok`**, and that is
    /// what keeps it wrong in one place rather than two. A subject that also
    /// rewrote the refusals would answer undecided for an identifier that
    /// was never stored and for a row the scope withheld, failing
    /// `converged-target-lookup`'s other two probes for a mistake about
    /// *absence* rather than about convergence.
    ///
    /// **What it reaches was measured rather than reasoned: that check's
    /// first probe, and nothing else.** No other check in the suite
    /// dispatches a lookup with `converged_only = true`, and the probe is
    /// individually load-bearing — neutering it leaves this subject's row
    /// empty. Of the check's other three, two report against
    /// [`Self::IgnoresScopeOnThePointRead`] and the third against nothing at
    /// all; that check's own docs carry the measurement and why the gap is
    /// recorded rather than closed.
    ///
    /// **It is also the only backend that drives the `Eventual` half of that
    /// probe.** The matrix runs every subject at
    /// [`DedupLevel`](super::DedupLevel)`::Linearizable`, where an undecided
    /// answer is a violation outright; under an `Eventual` declaration the
    /// check sleeps the declared bound and reads again, and a subject that
    /// never decides is what makes that second read happen. The test is
    /// `an_undecided_lookup_is_still_a_violation_once_the_bound_has_passed`,
    /// and neutering the post-bound assertion fails it and nothing else.
    AnswersNotConvergedForAnAcknowledgedEntry,
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
    /// reference by inverting it: the separate-call identity's own
    /// acceptance, the retry, the divergent submission, both "the earlier
    /// entry of a batch is accepted" assertions, and the identical in-batch
    /// pair's absorb. The seventh,
    /// [`Defect::BatchResolvesAgainstThePreCallLedger`], closed the divergent
    /// in-batch conflict and is individually load-bearing.
    ///
    /// Of the six, three are **structurally** out of reach and one is nearly
    /// so. A subject refusing the first entry of an identity fails most of
    /// the suite and is a caricature rather than a mistake anyone makes,
    /// which accounts for the separate-call acceptance and both "earlier
    /// entry accepted" assertions. The identical in-batch absorb is the
    /// third: an absorb and a second acceptance both answer `Ok` carrying an
    /// entry equal in every caller-supplied field, so no outcome tells them
    /// apart and only the row count can — which is the floor half, not that assertion.
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
    /// Answers every collision the conforming way and then writes the
    /// divergent submission over the row it just refused: last writer wins
    /// on the store even though the caller was told otherwise.
    ///
    /// DESIGN §3.1's "Dedup level" row is what this breaks, in two of its
    /// sentences at once and in neither of their *answers*: *"The first
    /// write in commit order is the **survivor**, and every read path, fold,
    /// reconciliation figure, materialised aggregate, and the feed show it
    /// and nothing else"*, and *"From then until retention frees the
    /// identity, no later write displaces the survivor and no outcome
    /// returned accepts divergent content"*. This subject keeps the second
    /// half of that last clause — no outcome it returns accepts divergent
    /// content — and breaks the first.
    ///
    /// It is the mistake a backend makes by writing `INSERT … ON CONFLICT
    /// (the six identity columns) DO UPDATE SET …` where the conforming
    /// statement is `DO NOTHING`, and then deciding absorb-or-conflict from
    /// the row `RETURNING` handed back. The decision is right, because the
    /// comparison is against the row as it stood; the store is wrong,
    /// because the same statement has already replaced it. A port written
    /// against an upsert-shaped table — the shape almost every other
    /// idempotent write in a system has — arrives here naturally.
    ///
    /// **Distinct from [`Self::AbsorbsAWithdrawalWithAnotherReason`]**,
    /// which is the other subject in this module about a divergent
    /// re-delivery. That one changes the *answer* and no row at all, and
    /// only for invalidations. This one changes the *row* and no answer at
    /// all, for any entry.
    ///
    /// **What it reaches was measured rather than reasoned: one assertion,
    /// in one check.** `dedup-concurrent`'s third probe reads a raced
    /// identity's content back after the race and requires the accepted
    /// entry, and neutering that probe leaves this subject's row empty.
    /// Nothing else in the suite reads content back after a divergent
    /// submission: `dedup-floor` re-reads its divergent identities, but by
    /// row count and by `COUNT`, both of which read identity and never
    /// content, and `at-most-one-invalidation` asserts the conflict's shape
    /// and stops there under a `linearizable` declaration.
    /// `server-field-round-trip`'s retry diverges only in `accepted_at` and
    /// `origin`, which [`UsageRecord::caller_supplied_eq`] does not read, so
    /// this subject absorbs it and writes nothing.
    ///
    /// **It is invisible to `dedup-concurrent`'s own `Eventual` half**, and
    /// that is a property of the displacement rather than an oversight: the
    /// row is replaced in place, so the ledger page, the `COUNT` and the
    /// feed all go on showing exactly one write per identity and all three
    /// show the same one. Under an `eventual` declaration no outcome names
    /// the survivor either, so nothing there can tell a displaced row from
    /// the one that was meant to win. The matrix runs every subject at
    /// `Linearizable`, where the outcomes do name it.
    ADivergentWriteDisplacesTheSurvivor,
    /// Folds `LATEST` on `(window_end, id)`, the middle key of DESIGN §3.1's
    /// three struck out.
    ///
    /// **This is the reference backend's own fold before it was corrected**,
    /// which is what makes it the plausible one rather than an invented
    /// wrongness. The two keys it keeps are the two a backend already has an
    /// index on: a ledger page is ordered by `(window_end, id)` throughout
    /// this SPI, and reaching for that same pair to settle a fold is the
    /// short step. The order it yields is still *total*, so nothing about the
    /// answer looks unreliable - it is simply the wrong entry whenever the
    /// middle key and the last disagree.
    ///
    /// DESIGN §3.1's `LATEST` tie-break is what it breaks: *"Greatest
    /// `window_end`, then greatest `accepted_at`, then greatest `id` in byte
    /// order."*
    ///
    /// **It reaches `latest-tie-break`'s `accepted_at` scenario and no
    /// other**, measured by neutering each of that check's three in turn. The
    /// `window_end` scenario separates on the first key, which this subject
    /// keeps; the cross-tenant scenario ties on the first two and falls to
    /// `id`, which this subject also keeps and which is DESIGN's own answer
    /// there.
    LatestSkipsTheAcceptanceInstant,
    /// Folds `LATEST` on `(window_end, accepted_at)` and settles what is left
    /// on arrival order - the last key of DESIGN §3.1's three struck out, so
    /// the order is no longer total.
    ///
    /// The mistake is not a substituted key but a **missing** one, and DESIGN
    /// states the consequence rather than leaving it to be inferred: `id` *"is
    /// unique, so the order is total"*. A backend whose `ORDER BY` stops at
    /// `accepted_at` answers whichever row its scan happened to reach last,
    /// so two entries a deployment cannot tell apart get an answer that
    /// depends on the storage layout. This is the class of mistake the
    /// `TimescaleDB` plugin makes by ordering on a counter that is monotonic
    /// per `(tenant_id, gts_type_id)` alone: inside one tenant it ranks
    /// something, and across a group spanning tenants it ranks nothing.
    ///
    /// **It reaches `latest-tie-break`'s cross-tenant scenario and no
    /// other**, measured the same way, and it is the only subject that
    /// reaches it: the other two both keep `id` and so both agree with DESIGN
    /// wherever the two keys above it tie. That scenario submits its two
    /// entries in descending `id` order precisely so that this subject's
    /// arrival tie-break picks the loser.
    LatestStopsAtTheAcceptanceInstant,
    /// Folds `LATEST` on `(accepted_at, id)`, the **first** key of DESIGN
    /// §3.1's three struck out.
    ///
    /// *Latest* reads as *most recently accepted*, and a backend that takes
    /// the fold's name at its word orders by the acceptance instant and stops
    /// thinking about the covered period. It is also where a port of a
    /// pre-period model lands: with no period column to order on, the
    /// acceptance instant is the only time a row carries.
    ///
    /// DESIGN puts the period end first, and the whole of §3.1's `Covered
    /// period` model is why: an entry states what it measured and *when the
    /// measurement covers*, which is not when the gear happened to accept it.
    ///
    /// **It reaches `latest-tie-break`'s `window_end` scenario and no
    /// other**, measured the same way. That scenario is built for it: its
    /// later-ending entry carries the *smaller* acceptance instant, so a fold
    /// that reads `accepted_at` first reports the other entry rather than
    /// agreeing by accident.
    LatestIgnoresThePeriodEnd,
    /// Advances a feed page's cursor past every entry the page **carried**
    /// rather than past every entry it **scanned** — the scope and the
    /// subscription gate moved above the cursor assignment.
    ///
    /// **It is the one-line change**, which is what makes it plausible: the
    /// admission decision and the cursor assignment sit in one loop body,
    /// and writing the cursor inside the `if` that already decides whether
    /// to carry the row reads as tidier than writing it outside. It still
    /// compiles, it still reads correctly under every single grant, and it
    /// passed the whole suite until `feed-snapshot-and-replay` landed.
    ///
    /// What it breaks is the position's **meaning**. DESIGN §3.1 fixes a
    /// position's age by the oldest subsequent entry of a subscribed type
    /// *"whether or not the reader's authorization scope admits that
    /// entry"*, and states that *"A page reaching the settled head returns
    /// its cursor at the head"*. Under this defect a position means "the
    /// last entry this grant admitted", so a cursor minted under one grant
    /// and resumed under a wider one silently skips every entry the narrower
    /// grant withheld ahead of it, and a walk whose tail is withheld never
    /// reaches the head at all.
    ///
    /// **The suite reaches it from one direction only**, and the direction
    /// matters because three others do not. Measured by neutering each of
    /// `feed-snapshot-and-replay`'s assertions in turn, the only one that
    /// reports is
    /// `a_wider_grant_resumes_a_narrower_walk_at_the_head`. The reason is in
    /// the mechanism: this backend's position is an entry's *sequence*
    /// rather than a count, so a lagging cursor names a coordinate the read
    /// really stood on. Nothing is re-delivered, no single-grant walk
    /// observes anything amiss, and two grants admitting the same entries
    /// are handed the same cursor. The cursor merely stops short — which is
    /// visible only to a reader whose grant admits something the walk's did
    /// not. `contract_tests`'
    /// `a_feed_position_denotes_the_same_ledger_prefix_under_every_grant`
    /// catches the same defect in the reference by comparing three grants'
    /// positions against a literal; this subject is what puts it in front of
    /// a check, where the comparison has to be a behavioural one.
    FeedCursorCountsAdmittedEntries,
    /// Resumes a feed page at the entry a position names instead of after
    /// it — `WHERE sequence >= :cursor` where the contract wants
    /// `> :cursor`.
    ///
    /// **The classic keyset off-by-one**, and the one every paginated read
    /// path is one character away from. DESIGN §3.3's plugin obligations
    /// state that *"Offset/limit scans are forbidden on both paginated
    /// paths"*, so the resumption has to be a key comparison — and a key
    /// comparison is exactly where the inclusive/exclusive choice is made
    /// and got wrong.
    ///
    /// What it costs is not a hang but a **stall**: every page after the
    /// first re-delivers the entry its own start position named and, at a
    /// page limit of one, carries nothing else. The cursor therefore never
    /// leaves that entry, so a consumer following it is handed one entry
    /// over and over while the rest of the ledger sits behind it. Two of
    /// DESIGN's clauses fail at once, which is honest rather than untidy:
    /// the entry repeats (an entry *appearing* where the scan had already
    /// been) and the entries after it never arrive (every one of them
    /// *disappearing*).
    ///
    /// `FeedStart::Oldest` is untouched: it resumes from nothing, and
    /// nothing minus one is still nothing.
    AFeedPageRedeliversTheEntryAtItsCursor,
    /// Mints a continuation on every feed page, including the closing page
    /// of a replay that has reached its `until`.
    ///
    /// [`FeedPage::next`] enumerates exactly two dispositions — *"`Some` on
    /// every page of a live read, short pages included; `None` once a
    /// bounded replay has reached its `until`"* — and this is the subject
    /// for the second of them. It is the disposition a backend is likeliest
    /// to miss, because minting a cursor is what every other page does and
    /// the closing page is the one exception.
    ///
    /// Nothing else about the replay changes: it carries the same entries
    /// and stops in the same place. What is lost is the **signal**. An
    /// absent `next` is the only thing that tells a caller a bounded replay
    /// is finished, so a replay that keeps minting one leaves a consumer
    /// following a cursor forever over a range it has already read whole.
    ABoundedReplayNeverCloses,
    /// Advances a feed page's cursor onto the entry the page stopped at, so
    /// that entry ends up **behind** the cursor instead of in front of it —
    /// the page limit checked one statement too late.
    ///
    /// **The classic short-page off-by-one, and the only feed defect here
    /// that loses an entry outright.** A backend that wants to tell a caller
    /// whether a next page exists fetches `limit + 1` rows and returns
    /// `limit` of them; the cursor it then mints has to come from the last
    /// row it **returned**, never from the last row it **fetched**. Taking
    /// it from the last row fetched is one subscript, it compiles, and it
    /// reads correctly under every limit the ledger never reaches — which is
    /// every test a porter writes with a page limit wider than the fixture
    /// set. Spelled as a loop, as it is here, it is the `break` moved below
    /// the cursor assignment rather than above it.
    ///
    /// What it breaks is the clause of DESIGN §3.1's Feed order invariant
    /// the other feed subjects leave alone: *"a page carries only settled
    /// entries — converged, with nothing more able to become visible before
    /// them — so no entry the read's compiled scope admits ever becomes
    /// visible behind a returned cursor, whatever the concurrency or commit
    /// order."* The entry it skips is settled, inside the subscription and
    /// inside the scope, and is now behind a cursor its consumer has already
    /// been handed: nothing delivers it again, and the usage it records is
    /// charged to nobody.
    ///
    /// **It reaches `feed-completeness` alone**, measured rather than
    /// reasoned, and the reason it does not also reach
    /// `feed-snapshot-and-replay` is that check's fixtures rather than its
    /// assertions. That check walks at a limit of one over a ledger that
    /// alternates an admitted tenant with a withheld one, so the entry every
    /// page of that walk skips is one the walking grant was never going to
    /// carry; its wider reads run at a limit its ledger never reaches. That
    /// is an immunity by construction, and worth knowing before either
    /// check's ledger is edited: a row growing here would not be evidence of
    /// a second mistake.
    AFeedPageDropsTheEntryAtItsLimit,
    /// Orders the feed by the gateway-stamped `accepted_at`, with the
    /// admission sequence beneath it so the key stays unique.
    ///
    /// DESIGN rules it out twice. §3.1's Feed order invariant opens *"One
    /// deterministic order over a subscription, realised by the plugin
    /// through `FeedPosition`. No other ordering is claimed,
    /// acceptance-instant order included"*, and §3.10's deployment-guide
    /// item 3 says what goes wrong: *"Feed order MUST NOT rest on
    /// gateway-stamped `accepted_at` alone: replica clock skew can stamp an
    /// invalidation earlier than its target."*
    ///
    /// It is the mistake a backend makes by having no feed-order column at
    /// all. `accepted_at` is already stored, already indexed for the
    /// reconciliation watermarks, already monotonic in every single-writer
    /// test, and it is the only field on a [`UsageRecord`] that looks like
    /// an arrival. A port that reaches for it is wrong only when two writers
    /// disagree about the clock — which is exactly when a correction can be
    /// stamped before the thing it corrects.
    ///
    /// **The key stays unique and the order stays total**, so nothing about
    /// this subject looks unreliable: a position still names one entry, a
    /// page still resumes where it left off, a replay still repeats itself,
    /// and a walk still reaches the head. What moves is where an
    /// invalidation sits relative to its target.
    ///
    /// **It reaches `feed-completeness`'s correction-order assertion
    /// alone**, and that assertion is the only one in the suite it could
    /// reach. Every other feed read in the suite is over entries that share
    /// one acceptance instant, or finds what it wants by `id` rather than by
    /// position - `server-field-round-trip` delivers a record and its
    /// invalidation off a feed page and looks each up by identifier, so a
    /// reordered page changes nothing it reads.
    FeedOrdersByTheAcceptanceInstant,
    /// Begins a bootstrap feed read at the head — `FeedStart::Oldest`
    /// delegated as `FeedStart::After(<the position the feed has reached>)`.
    ///
    /// **The mistake a backend makes by treating "no cursor supplied" as
    /// "start from now".** It is the natural shape when a feed is built on a
    /// change stream, a logical-replication slot or a `LISTEN`/`NOTIFY`
    /// channel: those hand out a position at the moment you subscribe, and
    /// "where the subscriber is" is the only position the machinery has. A
    /// port that reaches for it answers every resumed read correctly, which
    /// is every read a consumer that already holds a cursor ever makes — and
    /// then hands every *new* consumer an empty stream and a cursor at the
    /// head. DESIGN rules it out in the two places it defines the name:
    /// §3.1's `FeedStart` row has *"v1 admits these two, and neither begins
    /// at the head"*, and the SPI's `read_feed_page` doc has
    /// *"`FeedStart::Oldest` means the oldest position this plugin still
    /// serves for `subscription` under `scope` — never the head"*.
    ///
    /// **It is a wrapper, and it is the first feed defect that could be
    /// one.** The five before it land inside the page loop, so intercepting
    /// them would have meant re-implementing the loop; this one lands in how
    /// `start` is *interpreted* before the loop runs, which is exactly what
    /// an interception can replace. The head position it substitutes is read
    /// out of the inner backend — one unbounded page over the same
    /// subscription and scope, whose cursor is by definition the head — for
    /// want of any other way to name a position a plugin owns.
    ///
    /// Nothing else changes: `FeedStart::After` is delegated untouched, the
    /// page carries whatever the inner backend carries, and the cursor is
    /// the inner backend's own.
    AFeedBootstrapReadStartsAtTheHead,
    /// Refuses `FeedStart::Oldest` with `CursorBeyondRetention` once
    /// retention has swept a subscribed type — the cursor refusal applied to
    /// both arms of `FeedStart` instead of to one.
    ///
    /// **The mistake a backend makes by deciding the refusal before it looks
    /// at where the read begins.** `read_feed_page` has one retention check
    /// to make and two start modes to make it for, and the check reads a
    /// mark that says nothing about the caller: *"has retention removed an
    /// entry of a subscribed type after this position"*. Written once, above
    /// the branch, it is correct for `After` and wrong for `Oldest` — which
    /// asks for no particular continuation and so cannot have lost one.
    /// DESIGN §3.3's `feed-bootstrap-position` row says so in as many words:
    /// `FeedStart::Oldest` *"is never refused on the retention floor"*.
    ///
    /// What it costs is the worst failure this row has: a consumer that has
    /// to bootstrap **after** a sweep — which is every consumer that
    /// bootstraps at all on a backend old enough to have swept once — can
    /// never start. It cannot retry its way out either, because the mark
    /// only ever rises.
    ///
    /// **This subject is not in the discrimination matrix, and could not
    /// be.** Its defect needs a mark to exist, a mark needs a sweep, and a
    /// sweep needs a driver; `super::run_all` hands none, so under the
    /// matrix's dispatch this subject is behaviourally the reference backend
    /// and a row for it would assert the empty set. It is a row of
    /// `contract_tests`' `RETENTION_DRIVEN_MATRIX` instead, which asserts an
    /// empty column against `run_all` and its own against
    /// `run_all_with_retention`.
    ///
    /// The mark it keeps is its own rather than the inner backend's, which
    /// the wrapper cannot see. It records the type of every drop driven
    /// through it, which over-approximates a real mark: a drop that removed
    /// nothing raises no mark in the reference backend and records a type
    /// here. Nothing observes the difference, because every drop the suite
    /// drives over this subject removes an entry — and the
    /// over-approximation is in the direction that makes the subject *more*
    /// wrong, never less, so it cannot hide the defect it exists to show.
    RefusesTheOldestStartAfterASweep,
    /// Begins a bootstrap feed read one entry **past** the oldest entry a
    /// sweep left — the bootstrap position derived from the retention floor
    /// with the boundary the wrong side of it.
    ///
    /// **The mistake a backend makes by resolving `FeedStart::Oldest` out of
    /// its own retention marks.** That is a reasonable way to answer the
    /// question: a mark records the highest position of the entries a sweep
    /// removed — the `TimescaleDB` plugin's DESIGN gives
    /// `usage_feed_retention_marks` exactly that meaning — so "the oldest I
    /// still serve" is *after* the mark, and a read resolved that way is
    /// correct. It is correct only while the boundary is exclusive on the
    /// mark and inclusive on the first row above it, and that is the same
    /// inclusive/exclusive choice
    /// [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`] gets wrong on the
    /// resume path. Here it is got wrong on the bootstrap path, and in the
    /// other direction: one entry is skipped rather than repeated.
    ///
    /// **Nothing shows until a sweep has run.** With no mark there is no
    /// boundary to be on the wrong side of, so an undriven run meets the
    /// exemplar exactly. That is what makes this a second subject only a
    /// drive reaches, and it reaches the half of DESIGN's
    /// `feed-bootstrap-position` row that
    /// [`Defect::RefusesTheOldestStartAfterASweep`] does not: that one is
    /// refused and never answers, this one answers and begins in the wrong
    /// place. The row states both — *"begins at the oldest entry the
    /// subscription retains … and is never refused on the retention
    /// floor"* — and a subject for one is not a subject for the other.
    ///
    /// What it costs is an entry per sweep, silently. Every consumer that
    /// bootstraps after a sweep begins one entry late, and the entry it
    /// skipped is the oldest thing the backend still holds, so nothing will
    /// offer it again.
    ///
    /// The position it skips to is read out of the inner backend — one page
    /// at a limit of one from `Oldest`, whose cursor is the oldest retained
    /// entry's own position — for want of any way for a wrapper to see a
    /// sequence. A backend making this mistake for real reads it from its
    /// marks table.
    ///
    /// **Not in the discrimination matrix**, for the reason
    /// [`Defect::RefusesTheOldestStartAfterASweep`] is not.
    SkipsTheOldestEntryASweepLeft,
    /// Serves a cursor whose continuation retention has truncated as an
    /// ordinary page — the refusal left out altogether.
    ///
    /// **The failure DESIGN §3.3's `feed-retention-refusal` row names
    /// outright**: a cursor after which retention has removed an entry of a
    /// subscribed GTS type *"is refused rather than served as a short page"*.
    /// It is the mistake a backend makes by having no marks table at all —
    /// the sweep drops the rows, the feed goes on seeking past whatever
    /// position it is handed, and every read after a sweep is answered
    /// correctly *except* the ones that span it. A port arrives here by
    /// building the feed first and the retention interlock second, which is
    /// the order both of them get built in.
    ///
    /// What it costs is the only feed failure a consumer cannot see. The page
    /// is well formed, its cursor advances, and the entries swept away between
    /// the consumer's position and the ones it is handed are simply never
    /// delivered: the usage they record is charged to nobody and nothing says
    /// so. Every other way a feed can go wrong either refuses, repeats, or
    /// stalls.
    ///
    /// **Nothing shows until a sweep has run**, so [`mutant`] hands it out
    /// like any other subject and [`super::run_all`] finds it conforming. It
    /// is reached through [`drivable_ledger_mutant`] and
    /// [`super::run_all_with_retention`] instead, and `contract_tests`'
    /// `RETENTION_DRIVEN_MATRIX` is where its column is asserted.
    ServesAShortPageWhereRetentionTruncatedACursor,
    /// Decides the retention refusal from **what the caller's grant would
    /// have been delivered** rather than from what the sweep removed, so a
    /// cursor whose continuation lost only entries the caller could not read
    /// is served.
    ///
    /// **The clause it breaks is the sharpest one in the row**: a cursor is
    /// refused *"whether or not the caller's scope admitted that entry"*.
    /// `usage-collector-v1.yaml` draws the conclusion in as many words —
    /// removal *"is read per subscribed GTS type, so a cursor can be refused
    /// for an entry the caller's own scope excluded"*.
    ///
    /// It is the mistake a backend makes by keying its marks table on
    /// `(gts_type_id, tenant_id)` rather than on `gts_type_id`, which is the
    /// natural shape when every other table in the schema is keyed that way,
    /// and then joining the refusal to the caller's own tenant. Such a backend
    /// is right about every single-tenant subscription and wrong about
    /// everything else, so nothing in a single-tenant test suite finds it.
    ///
    /// What it costs is a silently truncated range for exactly the consumers
    /// whose scope narrows and widens — the case DESIGN §3.2 already warns
    /// leaves a gap no feed error marks. A consumer re-granted a tenant
    /// bootstraps to recover what it missed; a consumer served a short page
    /// here never learns there was anything to recover.
    ///
    /// This subject keeps the removed entries rather than only their highest
    /// sequence, which is what lets it evaluate the caller's scope against
    /// them. A real backend with a per-tenant marks table needs no such
    /// memory; the mirror keeps one because its mark is a sequence and a
    /// sequence carries no tenant.
    ///
    /// **Nothing shows until a sweep has run**, for the reason
    /// [`Self::ServesAShortPageWhereRetentionTruncatedACursor`] gives.
    TheRetentionRefusalReadsTheCallersGrant,
    /// Refuses every cursor over a subscription some sweep has touched,
    /// whatever that cursor's continuation still holds — the refusal decided
    /// from the **floor** rather than from what was removed.
    ///
    /// **The second sentence of DESIGN §3.3's row is what it breaks**: *"A
    /// cursor whose continuation is intact is served, including one older than
    /// the floor where the plugin retains longer than it."* A mark records what
    /// a sweep *actually removed*; a floor records what a sweep *would be
    /// permitted to remove*. They are not the same set, because DESIGN §3.1's
    /// "Plugin-owned lifecycle" row makes the horizon a floor rather than a
    /// boundary — *"A purge later than the horizon is permitted"* — so a
    /// conforming deployment holds entries below its own floor.
    ///
    /// It is the mistake a backend makes by storing the instant it last swept
    /// to and refusing any cursor issued before it, which needs no marks table
    /// and looks like the cheap version of one. It is also the mistake a
    /// backend makes by testing `mark IS NOT NULL` where the contract wants
    /// `mark > :position`.
    ///
    /// What it costs is every consumer at once, and permanently: a retention
    /// mark only ever rises, so from the first sweep onwards no consumer can
    /// resume and no consumer can retry its way out. It is the loudest of the
    /// retention-refusal subjects and the easiest to miss in review,
    /// because the refusal it returns is the contract's own variant.
    ///
    /// **Nothing shows until a sweep has run**, for the reason
    /// [`Self::ServesAShortPageWhereRetentionTruncatedACursor`] gives.
    RefusesEveryCursorOnceASweepHasRun,
    /// Reads its retention marks across every GTS type at once rather than
    /// per subscribed type, so a sweep over one meter refuses a cursor over
    /// another.
    ///
    /// `usage-collector-v1.yaml` states the obligation it breaks where it
    /// states the refusal itself: removal *"is read per subscribed GTS type,
    /// so a cursor can be refused for an entry the caller's own scope
    /// excluded"*. DESIGN §3.1 says the same thing from the position's side,
    /// fixing a position's age by the acceptance instant of *"the oldest entry
    /// of a subscribed GTS type after it"* — subscribed, rather than any type
    /// this backend holds.
    ///
    /// It is the mistake a backend makes by keeping **one** retention
    /// watermark rather than one row per type — the shape a deployment
    /// arrives at when every meter is swept on one timer and one instant
    /// describes the whole sweep — and it is also the mistake a backend makes
    /// by writing the marks table correctly and forgetting the
    /// `WHERE gts_type_id = ANY(:subscription)` on the read.
    ///
    /// What it costs is every consumer of every quiet meter. A meter nothing
    /// has been removed from is exactly the meter whose consumer has no reason
    /// to expect a refusal, and this backend refuses it the moment any other
    /// meter is swept — which, on a deployment where retention runs on a
    /// timer, is continuously.
    ///
    /// **Nothing shows until a sweep has run**, for the reason
    /// [`Self::ServesAShortPageWhereRetentionTruncatedACursor`] gives. What
    /// makes it visible after one is that `feed-retention-refusal` keeps a
    /// meter it never sweeps and reads a cursor over it **last**, after its
    /// own sweeps have raised a mark on the meter beside it.
    TheRetentionRefusalIgnoresTheSubscription,
    /// Keys its feed position **per tenant**: the position it issues carries
    /// one component for every tenant the subscribed GTS types hold entries
    /// under, so its encoded size grows with how many tenants the
    /// subscription spans.
    ///
    /// **DESIGN names this one in those words, twice.** §3.1's
    /// `FeedPosition` row: a position's *"**encoded size** is not
    /// [plugin-internal]: the gateway carries it inside a length-bounded wire
    /// cursor whose size may not grow with a subscription's breadth. A
    /// position keyed per tenant does not meet that bound, so a plugin needs
    /// a key it can compare across a whole subscription."* §3.10's
    /// deployment-guide item 3 repeats it as the thing a plugin author has
    /// to show is not the case.
    ///
    /// It is the mistake a backend makes by having a per-tenant sequence and
    /// no order above it — the shape this gear's own `TimescaleDB` plugin
    /// starts from, whose `usage_acceptance_sequence` keys its counter on
    /// `(tenant_id, gts_type_id)`. A backend in that position cannot issue
    /// one scalar that resumes a whole subscription, so it issues a vector:
    /// one component per tenant, and the position grows with the customer
    /// list.
    ///
    /// **It keys on the tenants the subscription's ledger holds, not on the
    /// tenants the caller's grant names**, and that is the only
    /// self-consistent version of this mistake rather than a choice. A
    /// position must denote the same ledger prefix under every grant (§3.1),
    /// so a position that named only the grant's tenants could not be resumed
    /// by a caller whose grant had since widened. Keying on the grant would
    /// be a second defect on top of this one, and it would put this subject
    /// in front of `feed-snapshot-and-replay`'s clause four as well — which
    /// that check's docs record as a gap with a named owner, and the owner
    /// is this check rather than this subject.
    AFeedPositionIsKeyedPerTenant,
    /// Keys its feed position **per subscribed GTS type**: one component for
    /// every type named in the subscription, so its encoded size grows as a
    /// consumer adds a meter.
    ///
    /// DESIGN does not name this one as an example, and it states the rule it
    /// breaks three times over without qualification: *"whose size may not
    /// grow with a subscription's breadth"* (§3.1), *"which may not grow with
    /// the breadth of the subscription it positions"* (§3.3), *"however wide
    /// a subscription grows"* (§3.10). A
    /// [`FeedSubscription`](crate::feed::FeedSubscription) **is** a set of
    /// GTS types, so a position carrying one component per type is the most
    /// literal reading of the growth those three forbid.
    ///
    /// It is the mistake a backend makes by keeping one feed-order sequence
    /// per meter — a partition per type is the natural physical layout, and a
    /// consumer subscribing to several of them then needs a cursor naming
    /// each. It is the same mistake as
    /// [`Self::AFeedPositionIsKeyedPerTenant`] made about the other axis, and
    /// it is a separate subject because the row's own contrast cannot see it:
    /// both of the subscriptions that contrast names one GTS type.
    AFeedPositionIsKeyedPerSubscribedType,
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
    if carries_its_own_ledger(defect) {
        return Box::new(MutantLedger::new(defect));
    }
    Box::new(WrappedReference::new(defect))
}

/// Which of the two shapes one defect takes: a ledger of its own, or a real
/// reference backend with one method intercepted.
///
/// The routing is a `match` rather than a default, and exhaustively: a defect
/// added and forgotten here is a compile error, where a wildcard would silently
/// give it whichever shape the fall-through named and a subject of the wrong
/// shape is a subject that does nothing. This module's header says which
/// defects take which shape and why.
///
/// It is a function rather than an arm inside [`mutant`] because two callers
/// need the answer and neither may guess it: [`mutant`] erases the subject
/// behind `Box<dyn UsageCollectorPluginV1>`, and a driven run needs the
/// concrete type so it can lend the same value as a [`ContractRetention`] too.
/// `contract_tests` asks this before choosing between [`drivable_mutant`] and
/// [`drivable_ledger_mutant`].
pub(super) fn carries_its_own_ledger(defect: Defect) -> bool {
    match defect {
        Defect::QuantityThroughFloat
        | Defect::StampsItsOwnAcceptedAt
        | Defect::DefaultsOriginToLive
        | Defect::DedupIgnoresThePeriod
        | Defect::DedupIgnoresTheEntryType
        | Defect::ConflictReadBackIgnoresTheEntryType
        | Defect::IgnoresScopeOnThePointRead
        | Defect::AnswersNotConvergedForAnAcknowledgedEntry
        | Defect::AbsorbsAWithdrawalWithAnotherReason
        | Defect::AFeedBootstrapReadStartsAtTheHead
        | Defect::RefusesTheOldestStartAfterASweep
        | Defect::SkipsTheOldestEntryASweepLeft
        | Defect::RefusesAWithdrawalWithTheSameReason
        | Defect::ConflictNamesTheRecord
        | Defect::EmptySumIsAbsent
        | Defect::CoalescesEveryEmptyFoldToZero
        | Defect::AGroupNothingSurvivesInStillGetsABucket
        | Defect::AFeedPositionIsKeyedPerTenant
        | Defect::AFeedPositionIsKeyedPerSubscribedType => false,
        Defect::SelectsOnWindowStart
        | Defect::FoldsTheInvalidation
        | Defect::LedgerHasNoUniqueConstraint
        | Defect::BatchResolvesAgainstThePreCallLedger
        | Defect::ADivergentWriteDisplacesTheSurvivor
        | Defect::LatestSkipsTheAcceptanceInstant
        | Defect::LatestStopsAtTheAcceptanceInstant
        | Defect::LatestIgnoresThePeriodEnd
        | Defect::FeedCursorCountsAdmittedEntries
        | Defect::AFeedPageRedeliversTheEntryAtItsCursor
        | Defect::ABoundedReplayNeverCloses
        | Defect::AFeedPageDropsTheEntryAtItsLimit
        | Defect::ServesAShortPageWhereRetentionTruncatedACursor
        | Defect::TheRetentionRefusalReadsTheCallersGrant
        | Defect::RefusesEveryCursorOnceASweepHasRun
        | Defect::TheRetentionRefusalIgnoresTheSubscription
        | Defect::FeedOrdersByTheAcceptanceInstant => true,
    }
}

// ---------------------------------------------------------------------------
// The wrapping shape
// ---------------------------------------------------------------------------

/// How many bytes one component of a keyed position carries: **sixteen**,
/// a tenant `Uuid`'s width.
///
/// One width for both keyed defects, so the arithmetic that strips them off
/// again is one rule rather than one per defect. The value is never read
/// back — `feed-position-bounded` compares how big a position is and never
/// what it says — so sixteen is the honest width of the thing a per-tenant
/// key actually names rather than a figure chosen to make a difference
/// visible.
const MUTANT_POSITION_COMPONENT_BYTES: usize = 16;

/// One position component standing for a subscribed GTS type.
///
/// The type id's first [`MUTANT_POSITION_COMPONENT_BYTES`] bytes, zero
/// padded. Two types sharing that prefix would share a component and the
/// subject would be none the worse for it: the components are counted,
/// never compared.
fn type_component(gts_type_id: &MeterTypeId) -> [u8; MUTANT_POSITION_COMPONENT_BYTES] {
    let mut component = [0_u8; MUTANT_POSITION_COMPONENT_BYTES];
    for (slot, byte) in component.iter_mut().zip(gts_type_id.as_str().as_bytes()) {
        *slot = *byte;
    }
    component
}

/// A real [`InMemoryReferencePlugin`] with one method intercepted.
///
/// Every path the defect is not about is the exemplar's own code, so a
/// failure against one of these subjects is a failure against a conforming
/// backend plus exactly the named mistake.
///
/// One qualification, and it holds for every wrapped defect:
/// [`Self::create_usage_records`] is not pure delegation. The inner backend
/// still decides the batch, but the per-entry alignment around it — which
/// entries reach it, and where a refusal of this wrapper's own lands in the
/// answer — is code written here. See that method's doc.
pub(super) struct WrappedReference {
    /// The conforming backend everything is delegated to.
    inner: InMemoryReferencePlugin,
    /// Which rule this subject breaks.
    defect: Defect,
    /// The period-blind dedup index [`Defect::DedupIgnoresThePeriod`] keys
    /// on: `(tenant_id, gts_type_id, idempotency_key, entry_type)` to the
    /// entry that claimed it. Unused by every other wrapped defect.
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
    /// window_end)` to the entry that claimed it. Unused by every other
    /// wrapped defect.
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
    /// entry accepted under them, in arrival order. Unused by every other
    /// wrapped defect.
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
    /// The GTS types retention has been driven over through this wrapper,
    /// which is the mark [`Defect::RefusesTheOldestStartAfterASweep`] reads.
    /// Unused by every other wrapped defect.
    ///
    /// The wire string rather than a [`MeterTypeId`], which implements
    /// neither `Ord` nor `PartialOrd`; the reference backend's own
    /// `retention_marks` is keyed the same way and for the same reason.
    ///
    /// A type is recorded whatever the drop removed, because a wrapper
    /// cannot see the inner backend's marks and has only the call to go on.
    /// That over-approximates a mark and the defect's own doc says what the
    /// over-approximation costs.
    swept_types: Mutex<BTreeSet<String>>,
    /// The tenants each GTS type has been written under, which is what
    /// [`Defect::AFeedPositionIsKeyedPerTenant`] issues one position
    /// component per. Unused by every other wrapped defect.
    ///
    /// The wire string keys it rather than a [`MeterTypeId`], which
    /// implements neither `Ord` nor `PartialOrd`; `swept_types` above is
    /// keyed the same way and for the same reason.
    ///
    /// **Recorded per type rather than in one set**, because that is the
    /// shape the defect is about: a position is issued for a subscription,
    /// and a per-tenant key names the tenants that subscription's types hold
    /// entries under. One set across the whole backend would make every
    /// subscription's position the same size, which is a backend that
    /// happens to pass the check it exists to fail.
    ///
    /// A tenant is recorded when the entry is admitted rather than after the
    /// inner backend stores it, like the two dedup indexes above, so a
    /// tenant whose only entry the inner backend then refused is still
    /// counted. That over-approximates the key set, in the direction of a
    /// longer position, and nothing reads it but the encoder.
    tenants_written: Mutex<BTreeMap<String, BTreeSet<Uuid>>>,
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
            swept_types: Mutex::new(BTreeSet::new()),
            tenants_written: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether retention has been driven over any type in `subscription`.
    ///
    /// The subscription is scanned rather than the whole set compared,
    /// because the reference backend's own refusal reads its marks *"per
    /// subscribed GTS type"* and a subject wrong about which types it
    /// consults would be wrong in a second way.
    fn a_subscribed_type_has_been_swept(
        &self,
        subscription: &[MeterTypeId],
    ) -> Result<bool, UsageCollectorPluginError> {
        let swept = self.swept_types.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's retention mark lock is poisoned")
        })?;
        Ok(subscription
            .iter()
            .any(|gts_type_id| swept.contains(gts_type_id.as_str())))
    }

    /// The position the feed has reached for `subscription` under `scope`,
    /// read out of the inner backend.
    ///
    /// One unbounded page at the widest limit there is: the inner backend
    /// advances its cursor past every entry it scans whether or not the
    /// page carries it, so a page that scanned the whole ledger hands back
    /// the head. [`Defect::AFeedBootstrapReadStartsAtTheHead`] substitutes
    /// it for `FeedStart::Oldest`.
    async fn the_head(
        &self,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
    ) -> Result<Option<FeedPosition>, UsageCollectorPluginError> {
        let page = self
            .inner
            .read_feed_page(subscription, scope, FeedStart::Oldest, None, u64::MAX)
            .await?;
        Ok(page.next)
    }

    /// The position just after the oldest entry `subscription` still
    /// retains, read out of the inner backend.
    ///
    /// One page at a limit of one from `FeedStart::Oldest`: the inner
    /// backend's cursor stops on the entry the page carried, so resuming
    /// after it is resuming one entry too late.
    /// [`Defect::SkipsTheOldestEntryASweepLeft`] substitutes it for
    /// `FeedStart::Oldest` once a sweep has run.
    async fn past_the_oldest_retained_entry(
        &self,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
    ) -> Result<Option<FeedPosition>, UsageCollectorPluginError> {
        let page = self
            .inner
            .read_feed_page(subscription, scope, FeedStart::Oldest, None, 1)
            .await?;
        Ok(page.next)
    }

    /// Records the tenant one entry is attributed to against its GTS type,
    /// and hands the entry on untouched.
    ///
    /// [`Defect::AFeedPositionIsKeyedPerTenant`]'s whole admission-side
    /// behaviour: it changes nothing about what is stored and only builds
    /// the key set its position encoder then issues one component per.
    ///
    /// Synchronous, and it holds the lock only for its own body: the caller
    /// awaits the inner backend afterwards, never while holding it.
    fn remember_the_tenant(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let mut written = self.tenants_written.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's tenant index lock is poisoned")
        })?;
        written
            .entry(record.gts_type_id.as_str().to_owned())
            .or_default()
            .insert(record.tenant_id);
        drop(written);
        Ok(record)
    }

    /// Whether this subject re-encodes the positions the inner backend
    /// issues.
    ///
    /// True for the two defects that key a position on something that grows
    /// with a subscription, false for every other wrapped defect — which
    /// hand the inner backend's own encoding straight back, so a position of
    /// theirs is the exemplar's byte for byte.
    fn keys_its_position(&self) -> bool {
        self.defect == Defect::AFeedPositionIsKeyedPerTenant
            || self.defect == Defect::AFeedPositionIsKeyedPerSubscribedType
    }

    /// The components this subject's position carries beside the inner
    /// backend's own encoding.
    ///
    /// One per tenant the subscribed types hold entries under, or one per
    /// type named — which of the two is the defect. The component *values*
    /// are never read back: only how many there are matters, because the
    /// rule is about a position's size. They are real values all the same,
    /// because a subject whose position carried filler would be a
    /// caricature of a plugin that has per-tenant state to name.
    fn position_components(
        &self,
        subscription: &[MeterTypeId],
    ) -> Result<Vec<[u8; MUTANT_POSITION_COMPONENT_BYTES]>, UsageCollectorPluginError> {
        if self.defect == Defect::AFeedPositionIsKeyedPerSubscribedType {
            return Ok(subscription.iter().map(type_component).collect());
        }
        if self.defect != Defect::AFeedPositionIsKeyedPerTenant {
            return Ok(Vec::new());
        }
        let written = self.tenants_written.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's tenant index lock is poisoned")
        })?;
        let mut tenants: BTreeSet<Uuid> = BTreeSet::new();
        for gts_type_id in subscription {
            if let Some(under_this_type) = written.get(gts_type_id.as_str()) {
                tenants.extend(under_this_type.iter().copied());
            }
        }
        drop(written);
        Ok(tenants.into_iter().map(Uuid::into_bytes).collect())
    }

    /// The position this subject hands a caller: its own components, then
    /// the inner backend's encoding.
    ///
    /// The component **count** leads, so the encoding is self-describing and
    /// [`Self::the_inner_position`] can strip it without knowing which
    /// subscription the position was issued under. A subject that recovered
    /// the count from the subscription instead would corrupt any position
    /// resumed under a different one, which is a second defect.
    fn the_issued_position(
        &self,
        subscription: &[MeterTypeId],
        inner: &FeedPosition,
    ) -> Result<FeedPosition, UsageCollectorPluginError> {
        if !self.keys_its_position() {
            return Ok(inner.clone());
        }
        let components = self.position_components(subscription)?;
        let count = u8::try_from(components.len()).map_err(|_| {
            UsageCollectorPluginError::internal(
                "this mutant's keyed position needs more components than its own one-byte count                  can carry",
            )
        })?;
        let mut bytes = Vec::with_capacity(
            1 + components.len() * MUTANT_POSITION_COMPONENT_BYTES + inner.len(),
        );
        bytes.push(count);
        for component in &components {
            bytes.extend_from_slice(component);
        }
        bytes.extend_from_slice(inner.as_bytes());
        FeedPosition::new(bytes).map_err(|err| {
            UsageCollectorPluginError::internal(format!(
                "this mutant's keyed position outgrew the published position bound, which makes                  it a subject about a refusal rather than about a size: {err}"
            ))
        })
    }

    /// The inner backend's own position, recovered from one this subject
    /// issued.
    ///
    /// The mirror of [`Self::the_issued_position`], and the reason the two
    /// defects are wrappers at all: everything the inner backend does with a
    /// resumed position is the exemplar's, because the position it is handed
    /// is the one it issued.
    fn the_inner_position(
        &self,
        issued: &FeedPosition,
    ) -> Result<FeedPosition, UsageCollectorPluginError> {
        if !self.keys_its_position() {
            return Ok(issued.clone());
        }
        let malformed = || {
            UsageCollectorPluginError::internal(
                "this mutant was handed a position it did not issue: its own encoding is a                  component count, that many components, and then the inner backend's position",
            )
        };
        let (count, rest) = issued.as_bytes().split_first().ok_or_else(malformed)?;
        let inner = rest
            .get(usize::from(*count) * MUTANT_POSITION_COMPONENT_BYTES..)
            .ok_or_else(malformed)?;
        FeedPosition::new(inner.to_vec()).map_err(|err| {
            UsageCollectorPluginError::internal(format!(
                "this mutant could not recover the inner backend's position from one of its own:                  {err}"
            ))
        })
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
            Defect::AFeedPositionIsKeyedPerTenant => self.remember_the_tenant(record),
            // Enumerated rather than caught by a wildcard. A new defect
            // routed here and forgotten would otherwise pass its
            // entries through untouched and report no violation at all;
            // spelling the variants out makes that a compile error instead
            // of a matrix row whose subject does nothing. The two point-read
            // defects are applied on the read path, the three withdrawal
            // defects and the conflict read-back around the inner call, the
            // three empty-selection defects on the fold, the three bootstrap
            // defects on the feed path, the per-subscribed-type position
            // defect on the way back out of it, and the ledger-carrying
            // defects never reach this type at all — `mutant` routes them to
            // `MutantLedger` — but exhaustiveness is the whole point.
            Defect::IgnoresScopeOnThePointRead
            | Defect::AnswersNotConvergedForAnAcknowledgedEntry
            | Defect::AbsorbsAWithdrawalWithAnotherReason
            | Defect::RefusesAWithdrawalWithTheSameReason
            | Defect::ConflictNamesTheRecord
            | Defect::ConflictReadBackIgnoresTheEntryType
            | Defect::SelectsOnWindowStart
            | Defect::FoldsTheInvalidation
            | Defect::EmptySumIsAbsent
            | Defect::CoalescesEveryEmptyFoldToZero
            | Defect::AGroupNothingSurvivesInStillGetsABucket
            | Defect::LedgerHasNoUniqueConstraint
            | Defect::BatchResolvesAgainstThePreCallLedger
            | Defect::ADivergentWriteDisplacesTheSurvivor
            | Defect::LatestSkipsTheAcceptanceInstant
            | Defect::LatestStopsAtTheAcceptanceInstant
            | Defect::LatestIgnoresThePeriodEnd
            | Defect::FeedCursorCountsAdmittedEntries
            | Defect::AFeedPageRedeliversTheEntryAtItsCursor
            | Defect::ABoundedReplayNeverCloses
            | Defect::AFeedPageDropsTheEntryAtItsLimit
            | Defect::AFeedBootstrapReadStartsAtTheHead
            | Defect::RefusesTheOldestStartAfterASweep
            | Defect::SkipsTheOldestEntryASweepLeft
            | Defect::ServesAShortPageWhereRetentionTruncatedACursor
            | Defect::TheRetentionRefusalReadsTheCallersGrant
            | Defect::RefusesEveryCursorOnceASweepHasRun
            | Defect::TheRetentionRefusalIgnoresTheSubscription
            | Defect::AFeedPositionIsKeyedPerSubscribedType
            | Defect::FeedOrdersByTheAcceptanceInstant => Ok(record),
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

    /// What the two post-inner defects **this method decides** do to one
    /// entry's outcome, and the place
    /// [`Defect::ConflictReadBackIgnoresTheEntryType`] keeps its mirror of
    /// the ledger up to date.
    ///
    /// There is a third post-inner defect and it is not here:
    /// [`Defect::ConflictNamesTheRecord`] needs an SPI read to answer, so it
    /// is applied by [`Self::conflict_against_the_withdrawn_record`] after
    /// this method returns.
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

    /// [`Defect::ConflictNamesTheRecord`]: a conflict raised for an entry
    /// carrying an invalidation is re-answered against the entry it
    /// withdraws.
    ///
    /// The target is **read back from the inner backend** rather than
    /// remembered here, so what the caller is handed is the exemplar's own
    /// stored row, server fields included - which is what a backend
    /// resolving `invalidates` in its conflict branch hands back. Nothing
    /// else about the outcome moves: an acceptance is passed through, and a
    /// conflict raised for a record has no target to substitute and is
    /// passed through too.
    ///
    /// Async, which is why it sits here rather than in
    /// [`Self::after_admission`] with the other two post-inner defects: that
    /// method is synchronous and the read-back is an SPI call. It is applied **after** that method on both
    /// create paths, so a conflict this subject rewrites is one the exemplar
    /// decided.
    ///
    /// A read-back that fails leaves the inner backend's own conflict in
    /// place. It is unreachable - an invalidation is only projectable
    /// against a target the store already holds, and the scope is that
    /// entry's own tenant - and a fall-through rather than an `expect` keeps
    /// a subject the matrix drives reporting the exemplar's answer if it
    /// ever stops holding.
    async fn conflict_against_the_withdrawn_record(
        &self,
        record: &UsageRecord,
        outcome: Result<UsageRecord, UsageCollectorPluginError>,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if self.defect != Defect::ConflictNamesTheRecord {
            return outcome;
        }
        let Err(UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing,
        }) = outcome
        else {
            return outcome;
        };
        let Some(invalidation) = record.invalidation.as_ref() else {
            return Err(UsageCollectorPluginError::IdempotencyConflict {
                idempotency_key,
                existing,
            });
        };
        match self
            .inner
            .get_usage_record(invalidation.target, &tenant_scope(record.tenant_id), true)
            .await
        {
            Ok(target) => Err(UsageCollectorPluginError::idempotency_conflict(
                idempotency_key,
                target,
            )),
            Err(_) => Err(UsageCollectorPluginError::IdempotencyConflict {
                idempotency_key,
                existing,
            }),
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

    /// [`Defect::AGroupNothingSurvivesInStillGetsABucket`]'s grouped answer:
    /// the exemplar's buckets, plus one for every key the range **selects**
    /// that none of them carries.
    ///
    /// The extra keys are read off the exemplar's own ledger page, which
    /// returns a withdrawn pair as persisted, so they are the keys a
    /// `GROUP BY` sees when the withdrawal exclusion sits in a
    /// `FILTER (WHERE …)` on the aggregate rather than in the `WHERE`.
    /// A key the grouping drops for a missing value is dropped here too,
    /// because [`bucket_key`] is the same function the mirror groups with —
    /// this subject is wrong about which groups exist, not about how one is
    /// keyed.
    ///
    /// Each added bucket carries the empty fold's own answer, taken from
    /// [`fold_value`] over no rows rather than spelled out again, so the
    /// subject is wrong in one way only: `SUM` and `COUNT` answer `0` there
    /// and the other three absent, exactly as the exemplar would have
    /// answered had the group existed and been emptied.
    ///
    /// The page is bounded by the caller's own `query.limit`, so a key
    /// carried only by rows past that limit is not added. Every grouped
    /// dispatch the suite makes reads a range of two rows under a limit of
    /// six, so nothing here rests on that; a check that grouped over a wider
    /// range would have to widen the limit with it.
    #[expect(
        clippy::too_many_arguments,
        reason = "the SPI's own aggregate signature, plus the delegated answer this rewrites; \
                  bundling them into a struct would name the SPI's parameters twice"
    )]
    async fn bucket_every_selected_key(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
        answered: AggregationResult,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let page = self
            .inner
            .list_usage_records(gts_type_id, time_range, query, metadata_filter)
            .await?;
        let mut buckets = answered.buckets;
        let mut present: BTreeSet<Vec<String>> =
            buckets.iter().map(|bucket| bucket.key.clone()).collect();
        let empty = fold_value(fold, &[], LatestOrder::Declared)?;
        for entry in &page.items {
            let Some(key) = bucket_key(entry, group_by) else {
                continue;
            };
            if present.insert(key.clone()) {
                buckets.push(AggregationBucket {
                    key,
                    value: empty.clone(),
                });
            }
        }
        Ok(AggregationResult { buckets })
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
        let outcome = self.after_admission(&record, outcome);
        self.conflict_against_the_withdrawn_record(&record, outcome)
            .await
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
        // A loop rather than a `map`, because one post-inner defect is
        // async: `conflict_against_the_withdrawn_record` reads the withdrawn
        // record back from the inner backend, and a closure cannot await.
        // The alignment is otherwise exactly what the `map` did - one inner
        // outcome consumed per entry this wrapper passed on, in order, and a
        // refusal of this wrapper's own kept where the entry stood.
        let mut answers = Vec::with_capacity(prepared.len());
        for entry in prepared {
            match entry {
                Ok(record) => {
                    let outcome = inner.next().unwrap_or_else(|| {
                        Err(UsageCollectorPluginError::internal(
                            "the inner backend answered fewer outcomes than the batch carried entries",
                        ))
                    });
                    let outcome = self.after_admission(&record, outcome);
                    answers.push(
                        self.conflict_against_the_withdrawn_record(&record, outcome)
                            .await,
                    );
                }
                Err(err) => answers.push(Err(err)),
            }
        }
        Ok(answers)
    }

    /// The point read, and the two defects that change what it answers.
    ///
    /// [`Defect::IgnoresScopeOnThePointRead`] substitutes the scope. `id eq
    /// <the id asked for>` is what `SELECT * FROM usage_records WHERE id =
    /// $1` compiles to: the scope argument is dropped and nothing else
    /// changes, which is the mistake as a backend makes it rather than a
    /// caricature of it.
    ///
    /// [`Defect::AnswersNotConvergedForAnAcknowledgedEntry`] leaves the
    /// query alone and rewrites one answer: a converged-only lookup that
    /// found the entry reports it undecided instead. **Only that answer**,
    /// which is what confines the subject to one mistake — a refusal is
    /// passed through, so an identifier that was never stored and a row the
    /// scope withheld both still read as absent, and this subject is wrong
    /// about convergence rather than about absence. See the defect for what
    /// it reaches.
    ///
    /// The two are exclusive branches rather than one composed path. Each
    /// subject carries exactly one defect, so composing them would be dead
    /// code dressed as generality.
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
        let answer = self.inner.get_usage_record(id, scope, converged_only).await;
        if converged_only
            && answer.is_ok()
            && self.defect == Defect::AnswersNotConvergedForAnAcknowledgedEntry
        {
            return Err(UsageCollectorPluginError::UsageRecordNotConverged { id });
        }
        answer
    }

    /// Delegated, with the three empty-selection defects rewriting what
    /// comes back.
    ///
    /// Each touches a different part of the answer, and each leaves every
    /// non-empty bucket the exemplar computed exactly as it computed it:
    ///
    /// * [`Defect::EmptySumIsAbsent`] — the **value** of a `SUM` bucket
    ///   whose selection was empty.
    /// * [`Defect::CoalescesEveryEmptyFoldToZero`] — the value of a `MAX`,
    ///   `MIN` or `LATEST` bucket the exemplar answered absent, which over
    ///   these three folds is exactly an empty selection.
    /// * [`Defect::AGroupNothingSurvivesInStillGetsABucket`] — which
    ///   **buckets** a grouped fold emits at all. See
    ///   [`Self::bucket_every_selected_key`].
    ///
    /// **A second dispatch is what tells an empty `SUM` bucket from one
    /// whose rows sum to zero**, and the two have to be told apart:
    /// `SUM(x)` over no rows is `NULL` in SQL while `SUM(x)` over rows that
    /// cancel is `0`, so a subject that answered absent to both would be
    /// wrong about a rule no check here asks it about. The exemplar's own
    /// `COUNT` over the same arguments is the signal — same selection, same
    /// exclusion, same grouping, so its buckets carry the same keys and a
    /// zero count is exactly an empty selection. Reading the `SUM` result
    /// alone cannot do it, and re-implementing the fold here would make this
    /// a [`MutantLedger`] rather than a wrapper. The other two folds need no
    /// probe: an absent `MAX` *is* an empty selection, and a missing bucket
    /// is read off the ledger page instead.
    ///
    /// The `SUM` rewrite is keyed on the bucket key rather than on position,
    /// so it does not rest on two calls returning their buckets in one
    /// order. It therefore reaches the ungrouped bucket and, in principle, a
    /// grouped one — but no grouped bucket the exemplar emits is ever empty,
    /// because a group is keyed from a surviving row, so in practice it only
    /// touches the no-grouping case. That is the whole of the clause it
    /// breaks.
    async fn query_aggregated_usage_records(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let result = self
            .inner
            .query_aggregated_usage_records(
                gts_type_id.clone(),
                time_range,
                fold,
                query,
                metadata_filter,
                group_by,
            )
            .await?;
        match self.defect {
            Defect::EmptySumIsAbsent if fold == AggregationFold::Sum => {
                let counted = self
                    .inner
                    .query_aggregated_usage_records(
                        gts_type_id,
                        time_range,
                        AggregationFold::Count,
                        query,
                        metadata_filter,
                        group_by,
                    )
                    .await?;
                let zero = BigDecimal::from(0);
                let empty: BTreeSet<Vec<String>> = counted
                    .buckets
                    .into_iter()
                    .filter(|bucket| bucket.value.as_ref() == Some(&zero))
                    .map(|bucket| bucket.key)
                    .collect();
                Ok(AggregationResult {
                    buckets: result
                        .buckets
                        .into_iter()
                        .map(|mut bucket| {
                            if empty.contains(&bucket.key) {
                                bucket.value = None;
                            }
                            bucket
                        })
                        .collect(),
                })
            }
            Defect::CoalescesEveryEmptyFoldToZero
                if matches!(
                    fold,
                    AggregationFold::Max | AggregationFold::Min | AggregationFold::Latest
                ) =>
            {
                Ok(AggregationResult {
                    buckets: result
                        .buckets
                        .into_iter()
                        .map(|mut bucket| {
                            if bucket.value.is_none() {
                                bucket.value = Some(BigDecimal::from(0));
                            }
                            bucket
                        })
                        .collect(),
                })
            }
            Defect::AGroupNothingSurvivesInStillGetsABucket if !group_by.is_empty() => {
                self.bucket_every_selected_key(
                    gts_type_id,
                    time_range,
                    fold,
                    query,
                    metadata_filter,
                    group_by,
                    result,
                )
                .await
            }
            _ => Ok(result),
        }
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

    /// Delegated, with `FeedStart::Oldest` reinterpreted by the bootstrap
    /// defects and the page's continuation re-encoded by the two that are
    /// about what a position costs to carry.
    ///
    /// The bootstrap defects land **above** the page loop rather than inside
    /// it, which is why they can be interceptions at all where the feed
    /// defects in [`MutantLedger`] cannot: two substitute a different
    /// `start` and one declines to serve the call. Everything after that
    /// decision is the exemplar's own loop, cursor and page.
    ///
    /// The position defects land **below** it, on the position the loop
    /// produced, and are interceptions for the mirror of that reason: a
    /// position's encoding is a function of the position, so there is
    /// nothing of the loop to re-implement. [`Self::the_inner_position`]
    /// strips a subject's own components off whatever it is handed and
    /// [`Self::the_issued_position`] puts them back on the way out, so the
    /// inner backend is resumed at exactly the position it issued and what
    /// differs is the size of the token a caller carries. Both are no-ops
    /// for every other wrapped defect.
    ///
    /// `FeedStart::After` is delegated untouched by the bootstrap defects.
    /// It is matched with a wildcard arm because [`FeedStart`] is
    /// `#[non_exhaustive]`, and a start mode added later is one neither of
    /// them has an opinion about.
    async fn read_feed_page(
        &self,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition>, UsageCollectorPluginError> {
        let bootstrapping = matches!(start, FeedStart::Oldest);

        if bootstrapping
            && self.defect == Defect::RefusesTheOldestStartAfterASweep
            && self.a_subscribed_type_has_been_swept(subscription)?
        {
            return Err(UsageCollectorPluginError::CursorBeyondRetention);
        }

        let substituted =
            if bootstrapping && self.defect == Defect::AFeedBootstrapReadStartsAtTheHead {
                self.the_head(subscription, scope).await?
            } else if bootstrapping
                && self.defect == Defect::SkipsTheOldestEntryASweepLeft
                && self.a_subscribed_type_has_been_swept(subscription)?
            {
                self.past_the_oldest_retained_entry(subscription, scope)
                    .await?
            } else {
                None
            };
        // The inner backend carries a continuation on every live page, so
        // `None` from either probe is unreachable through it. Delegating the
        // original start is the only answer to it that is not a second
        // defect.
        let start = match substituted {
            Some(position) => FeedStart::After(position),
            None => start,
        };

        // A position this subject issued is the inner backend's with this
        // subject's own components in front of it, so both of the positions
        // a read can be handed are stripped back before the inner backend
        // ever sees them.
        let start = match start {
            FeedStart::After(position) => FeedStart::After(self.the_inner_position(&position)?),
            other => other,
        };
        let until = match until {
            Some(position) => Some(self.the_inner_position(&position)?),
            None => None,
        };

        let page = self
            .inner
            .read_feed_page(subscription, scope, start, until, limit)
            .await?;
        let next = match page.next {
            Some(position) => Some(self.the_issued_position(subscription, &position)?),
            None => None,
        };
        Ok(FeedPage {
            entries: page.entries,
            next,
        })
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

/// The retention drive, delegated to the exemplar and recorded.
///
/// Every wrapped subject implements it rather than only the one defect that
/// reads the record, for the reason every wrapped subject carries the three
/// dedup indexes it may not use: a capability that exists on the shape means
/// any defect routed here can be handed to
/// [`run_all_with_retention`](super::run_all_with_retention) without first
/// growing a second shape. The sweep itself is the exemplar's, so a subject
/// under a drive is still the reference backend plus exactly its own named
/// mistake.
#[async_trait]
impl ContractRetention for WrappedReference {
    /// Records the type and hands the drive to the inner backend.
    ///
    /// The record is taken **before** the delegation for the same reason the
    /// two dedup indexes claim a key before the inner call does: it is a
    /// write inside the same transaction as the sweep, and a mark a backend
    /// raised for a sweep that then failed is a mark a real backend would
    /// also be left holding.
    ///
    /// # Errors
    ///
    /// Whatever the inner backend answers, plus a poisoned-lock report of
    /// this wrapper's own.
    async fn drop_before(
        &self,
        gts_type_id: &MeterTypeId,
        floor: time::OffsetDateTime,
    ) -> Result<(), String> {
        self.swept_types
            .lock()
            .map_err(|_| "the mutant's retention mark lock is poisoned".to_owned())?
            .insert(gts_type_id.as_str().to_owned());
        self.inner.drop_before(gts_type_id, floor).await
    }
}

/// The subject one defect names, as a concrete type a driven run can hold.
///
/// [`mutant`] erases its subject behind `Box<dyn UsageCollectorPluginV1>`,
/// which is right for the discrimination matrix and useless for
/// [`run_all_with_retention`](super::run_all_with_retention): that entry
/// point wants the same backend as a [`ContractRetention`] too, and a value
/// erased behind one of the two traits cannot be recovered as the other.
/// This returns the wrapper itself, so a caller can lend it as both.
///
/// Only the wrapped shape is offered. [`MutantLedger`] implements no
/// retention and none of its defects is about one.
pub(super) fn drivable_mutant(defect: Defect) -> WrappedReference {
    WrappedReference::new(defect)
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

/// How many admission sequences [`MutantLedger::feed_key`] reserves under
/// each whole second of a gateway-stamped acceptance instant.
///
/// Used by one subject alone, [`Defect::FeedOrdersByTheAcceptanceInstant`],
/// to keep a composite key unique. A power of two rather than a round
/// decimal because nothing reads it back — it is headroom, not a unit — and
/// a million sequences inside one second is headroom this suite exceeds by
/// no imaginable margin.
const SEQUENCES_PER_SECOND: u64 = 1 << 20;

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
    /// **Raised by [`Self::drop_before`], which landed with
    /// `feed-retention-refusal`.** It stayed empty while nothing could drive a
    /// sweep against this type, and the refusal in [`MutantLedger`]'s
    /// `read_feed_page` never fired — which was exactly the answer the
    /// reference gives for a backend nobody has driven. A subject handed to
    /// [`super::run_all`] is still in that state, because that entry point
    /// drives nothing.
    retention_marks: BTreeMap<String, u64>,
    /// The entries a sweep removed, with the sequences they were admitted
    /// under.
    ///
    /// **Read by one defect alone**,
    /// [`Defect::TheRetentionRefusalReadsTheCallersGrant`], which decides the
    /// refusal from what the caller's grant would have carried and therefore
    /// needs the rows rather than a high-water mark. The reference backend
    /// keeps no such list and needs none: its refusal reads
    /// [`Self::retention_marks`] and nothing else, which is the whole of what
    /// DESIGN asks for.
    ///
    /// It is a mirror of what a sweep took rather than a second ledger: no
    /// read path consults it, nothing is ever removed from it, and a subject
    /// that does not carry that defect never looks at it.
    removed: Vec<Entry>,
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

    /// Writes `record` over the entry already stored under its `id`,
    /// **keeping the sequence that entry was admitted with**.
    ///
    /// [`Defect::ADivergentWriteDisplacesTheSurvivor`]'s whole effect, and
    /// the only place this ledger mutates an entry rather than appending
    /// one. The sequence is kept because the defect is about *which write*
    /// an identity holds and not about where that identity sits in the
    /// feed's order: restamping it would move the row on the feed as well,
    /// which is a second mistake in a subject that must be wrong in exactly
    /// one.
    ///
    /// An `id` no entry carries is a no-op. The one caller reaches this
    /// only on a collision, which already established that a row under that
    /// `id` is there.
    fn replace(&mut self, record: UsageRecord) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.record.id == record.id)
        {
            entry.record = record;
        }
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

    /// Removes every entry of `gts_type_id` whose covered period ends before
    /// `floor`, raising that type's mark to the highest sequence removed, as
    /// the reference's sweep does.
    ///
    /// The bound is exclusive and the mark is raised rather than assigned, so
    /// a later drop at a lower floor cannot lower it. Sequences are stamped in
    /// admission order and never reissued, so a mark goes on meaning the same
    /// thing against a position issued before the drop.
    ///
    /// The removed entries are kept in [`Self::removed`] as well, which the
    /// reference does not do; that field says which single defect reads them
    /// and why the reference needs no such list.
    fn drop_before(&mut self, gts_type_id: &MeterTypeId, floor: time::OffsetDateTime) {
        let mut kept = Vec::with_capacity(self.entries.len());
        for entry in std::mem::take(&mut self.entries) {
            if entry.record.gts_type_id != *gts_type_id || entry.record.window_end >= floor {
                kept.push(entry);
                continue;
            }
            let mark = self
                .retention_marks
                .entry(gts_type_id.as_str().to_owned())
                .or_default();
            *mark = (*mark).max(entry.sequence);
            self.removed.push(entry);
        }
        self.entries = kept;
    }

    /// Whether any subscribed type carries a retention mark at all, whatever
    /// that mark is and whatever `position` a cursor names.
    ///
    /// [`Defect::RefusesEveryCursorOnceASweepHasRun`] reads this in place of
    /// [`Self::retention_has_passed`], which is the whole of that defect: the
    /// comparison against the position is what separates a truncated
    /// continuation from an intact one, and dropping it refuses both.
    fn a_subscribed_type_has_been_swept(&self, subscription: &[MeterTypeId]) -> bool {
        subscription
            .iter()
            .any(|gts_type_id| self.retention_marks.contains_key(gts_type_id.as_str()))
    }

    /// Whether retention has removed an entry strictly after `position` on
    /// **any** type, subscribed or not.
    ///
    /// [`Defect::TheRetentionRefusalIgnoresTheSubscription`] reads this in
    /// place of [`Self::retention_has_passed`]. The comparison against the
    /// position is the conforming one; what is dropped is the subscription,
    /// which `usage-collector-v1.yaml` makes the unit the refusal is read
    /// over.
    fn retention_has_passed_on_any_type(&self, position: u64) -> bool {
        self.retention_marks.values().any(|mark| *mark > position)
    }

    /// Whether retention removed an entry after `position` that `scope` would
    /// have admitted, on a subscribed type.
    ///
    /// [`Defect::TheRetentionRefusalReadsTheCallersGrant`] reads this in place
    /// of [`Self::retention_has_passed`]. Everything about it is the
    /// conforming rule except the last conjunct, which is the one DESIGN
    /// forbids: the refusal is read from what a sweep removed, never from what
    /// this caller's grant would have been handed.
    fn retention_removed_something_this_grant_admits(
        &self,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
        position: u64,
    ) -> bool {
        self.removed.iter().any(|entry| {
            entry.sequence > position
                && subscription.contains(&entry.record.gts_type_id)
                && expr_admits(&entry.record, scope)
        })
    }
}

/// A ledger of this module's own, for the defects a wrapper cannot reach.
///
/// Two of them change a predicate the inner backend owns — which column a
/// range meets, and which rows a fold walks — so there is no method to
/// intercept. Three more change what the inner backend *stores*: a
/// duplicate row under one identity, a batch decided against a snapshot, and
/// a row overwritten by the submission that collided with it. A wrapper
/// cannot make [`InMemoryReferencePlugin`] hold or move a row its own
/// admission refuses to. The rest are the `LATEST` orders, the page loop and
/// the refusal guarding it, which this module's header groups.
///
/// Everything else mirrors [`InMemoryReferencePlugin`]: the same
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
/// **The feed's four originally unpinned behaviours are now all pinned**,
/// three of them by `feed-snapshot-and-replay` and the fourth by
/// `feed-retention-refusal`. Each was measured after
/// that check landed, the same way the rest were — broken here, then the
/// matrix run:
///
/// * **The scanned-entry cursor rule** is pinned, and it now has a subject
///   of its own rather than only a measurement:
///   [`Defect::FeedCursorCountsAdmittedEntries`].
/// * **The scope gate** is pinned. A feed answering every tenant's entries
///   under any grant delivers, into that check's walk, entries under a
///   tenant its walking grant does not name.
/// * **The subscription gate** is pinned. A feed answering every meter's
///   entries delivers, into the same walk, entries every other check left on
///   the suite's shared meter.
/// * **The seek was unpinned until `feed-retention-refusal` landed**, and
///   why it was is worth keeping: replacing `sequence > resumed_at` with a
///   `skip` of that many rows left the matrix green, because this backend's
///   sequences are dense over one append-ordered ledger and a count of rows
///   past the start names the same entry a seek to the first greater
///   sequence does. **Only a gap separates them**, a retention drop is what
///   makes one, and that check is the one that reads across a gap. Measured
///   again with the skip in place, the driven run now reports it — through
///   `contract_tests`'
///   `a_subject_carrying_its_own_ledger_is_still_itself_under_a_drive`,
///   whose subject is chosen for touching no feed path of its own, and
///   through that check's assertion that a cursor whose continuation is
///   intact is served: under a skip the walk from such a cursor never
///   reaches the head. The undriven matrix stays green under the same
///   change, which is the measurement rather than the design intent. The
///   `>` itself was already pinned:
///   [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`] is the subject for
///   getting that comparison wrong.
///
/// **Two more of the feed's decisions were pinned when `feed-completeness`
/// landed**, and each got a subject rather than only a measurement. The
/// **order** this type walks its ledger in is pinned by
/// [`Defect::FeedOrdersByTheAcceptanceInstant`], and **where the cursor
/// stops relative to the page limit** by
/// [`Defect::AFeedPageDropsTheEntryAtItsLimit`]. The second is worth a note
/// for whoever edits a feed check's ledger next: it is invisible to
/// `feed-snapshot-and-replay` because that check's walk runs at a limit of
/// one over a ledger alternating an admitted tenant with a withheld one, so
/// the entry each of its pages steps over is one its grant withheld anyway.
/// That immunity is a property of those fixtures, not of those assertions.
///
/// **The feed's retention refusal is pinned too, and its sweep with it.**
/// Both used to be unpinned because no check drove a drop — measured by
/// disabling the refusal outright and watching the matrix stay green — and
/// `feed-retention-refusal` is what changed that. It is pinned only under
/// [`super::run_all_with_retention`], because [`super::run_all`] still
/// drives nothing: the test that spends it is
/// `a_subject_carrying_its_own_ledger_is_still_itself_under_a_drive`, and a
/// reader should not take a green undriven matrix as covering any of it.
///
/// Everything else here is level with the reference and **unpinned**: the
/// ledger page's *order* (its membership is pinned, its sort is asserted by
/// no check), the grouped folds and the three that read a quantity, and the
/// reconciliation figures. They are mirrored because the checks that read
/// them are coming, and each becomes pinned by the check that first
/// dispatches it. An unpinned behaviour is where this mirror can still rot
/// in silence, which is the argument for keeping it level now rather than
/// letting it answer `Internal` until someone needs it.
pub(super) struct MutantLedger {
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

    /// The ledger in the order this subject's feed walks it, each entry
    /// beside the key its positions encode.
    ///
    /// For every subject but one that is the admission sequence, ascending,
    /// which is the `Vec`'s own order and so needs no sort: a sequence is
    /// stamped at admission and only ever rises.
    /// [`Defect::FeedOrdersByTheAcceptanceInstant`] is the exception and the
    /// only reason this method exists — an order is not a line of code, so
    /// that subject could not be a branch inside the page loop the way the
    /// other four feed subjects are.
    fn feed_order<'ledger>(&self, ledger: &'ledger Ledger) -> Vec<(u64, &'ledger Entry)> {
        let mut ordered: Vec<(u64, &Entry)> = ledger
            .entries
            .iter()
            .map(|entry| (self.feed_key(entry), entry))
            .collect();
        if self.defect == Defect::FeedOrdersByTheAcceptanceInstant {
            ordered.sort_by_key(|(key, _)| *key);
        }
        ordered
    }

    /// The key one entry takes in this subject's feed order, which is also
    /// the value its [`FeedPosition`] encodes.
    ///
    /// [`Defect::FeedOrdersByTheAcceptanceInstant`] keys on the
    /// gateway-stamped instant with the admission sequence beneath it. The
    /// sequence is there so the key stays **unique**: a defect that made two
    /// entries share a position would make this backend unwalkable rather
    /// than mis-ordered, and this subject is meant to be wrong about order
    /// alone.
    ///
    /// [`SEQUENCES_PER_SECOND`] is the room the low half reserves. The suite
    /// writes a few hundred entries in all, so a second's worth of sequences
    /// is never exhausted; `saturating_mul` stops a clock outside the fixture
    /// vocabulary from wrapping rather than making this subject correct for
    /// such a clock.
    ///
    /// One consequence is worth naming: under this subject a position is a
    /// composite key rather than a sequence, while [`Ledger`]'s retention
    /// marks are sequences. Comparing the two would mean nothing, and it
    /// never happens under [`super::run_all`], which drives no sweep at all.
    /// Under [`super::run_all_with_retention`] it could, and this subject is
    /// simply never handed to that entry point: it is a row of
    /// `contract_tests`' undriven discrimination matrix and of no driven
    /// test. Whoever drives it first has to reconcile the two keys here
    /// before reading anything into the result.
    fn feed_key(&self, entry: &Entry) -> u64 {
        if self.defect != Defect::FeedOrdersByTheAcceptanceInstant {
            return entry.sequence;
        }
        u64::try_from(entry.record.accepted_at.unix_timestamp())
            .unwrap_or(0)
            .saturating_mul(SEQUENCES_PER_SECOND)
            .saturating_add(entry.sequence)
    }

    /// Decides one entry and writes it, under this subject's own admission
    /// rule.
    ///
    /// Every subject but [`Defect::LedgerHasNoUniqueConstraint`] and
    /// [`Defect::ADivergentWriteDisplacesTheSurvivor`] admits through
    /// [`admit`], which is the reference's decision: a collision on `id`
    /// resolves by caller-supplied fields and only a fresh entry is written.
    /// The first of those two admits through
    /// [`admit_without_a_unique_constraint`], which decides the same way and
    /// writes regardless; the second through
    /// [`admit_displacing_the_survivor`], which decides the same way and
    /// writes the refused submission over the row it was refused against.
    ///
    /// **This is why those two defects carry a ledger of their own rather
    /// than wrapping the reference.** The two predicate defects routed here
    /// change something the inner backend decides; these two change what it
    /// *stores*, and a wrapper cannot make [`InMemoryReferencePlugin`] hold
    /// a second row under one `id`, nor write one row over another - its own
    /// admission refuses both. A wrapper keeping the extra or the replaced
    /// rows in a side ledger would then have to re-implement every read
    /// path's selection, ordering and paging to merge them back in, which is
    /// this type with extra steps.
    fn admit_here(
        &self,
        ledger: &mut Ledger,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if self.defect == Defect::LedgerHasNoUniqueConstraint {
            return admit_without_a_unique_constraint(ledger, record);
        }
        if self.defect == Defect::ADivergentWriteDisplacesTheSurvivor {
            return admit_displacing_the_survivor(ledger, record);
        }
        admit(ledger, record)
    }

    /// Whether this subject refuses a cursor at `position`, and the one place
    /// the retention-refusal defects live.
    ///
    /// The conforming rule is the reference's: refuse when some subscribed
    /// type's retention mark is beyond the position, which reads what a sweep
    /// removed and nothing else. Each defect substitutes a different
    /// question:
    ///
    /// * [`Defect::ServesAShortPageWhereRetentionTruncatedACursor`] asks
    ///   nothing and never refuses.
    /// * [`Defect::TheRetentionRefusalReadsTheCallersGrant`] asks what the
    ///   caller's grant would have carried.
    /// * [`Defect::RefusesEveryCursorOnceASweepHasRun`] asks only whether a
    ///   sweep ran, dropping the comparison against the position.
    /// * [`Defect::TheRetentionRefusalIgnoresTheSubscription`] asks it of
    ///   every type at once rather than of the subscribed ones.
    ///
    /// A wildcard rather than an enumeration, unlike
    /// [`WrappedReference::on_admission`]'s, and the direction is why: there
    /// the fall-through passes an entry untouched, so a defect routed there
    /// and forgotten would do nothing and report nothing. Here the
    /// fall-through is the *conforming* rule, so a defect routed here and
    /// forgotten behaves as the exemplar does — which is what every defect
    /// this method is not about already wants.
    fn refuses_the_cursor(
        &self,
        ledger: &Ledger,
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
        position: u64,
    ) -> bool {
        match self.defect {
            Defect::ServesAShortPageWhereRetentionTruncatedACursor => false,
            Defect::TheRetentionRefusalReadsTheCallersGrant => {
                ledger.retention_removed_something_this_grant_admits(subscription, scope, position)
            }
            Defect::RefusesEveryCursorOnceASweepHasRun => {
                ledger.a_subscribed_type_has_been_swept(subscription)
            }
            Defect::TheRetentionRefusalIgnoresTheSubscription => {
                ledger.retention_has_passed_on_any_type(position)
            }
            _ => ledger.retention_has_passed(subscription, position),
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
    /// The three `LATEST` defects change nothing about which rows reach the
    /// fold and everything about which of them it ranks highest: they travel
    /// as a [`LatestOrder`] into [`fold_rows`] and are read by that function
    /// alone. A fold defect is the one kind this type can carry without
    /// touching its own selection, which is why the row filter above is
    /// blind to all three.
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
        fold_rows(fold, &rows, group_by, LatestOrder::of(self.defect))
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
    /// pins for the reference and
    /// [`Defect::FeedCursorCountsAdmittedEntries`] is the subject that puts
    /// it in front of a check here. And a cursor whose continuation retention
    /// has truncated is refused, which is DESIGN §3.3's
    /// `feed-retention-refusal`; [`Ledger::drop_before`] is what raises the
    /// mark it reads, and only [`super::run_all_with_retention`] drives one.
    ///
    /// **Nine defects routed here touch the feed.** Five cover the four
    /// decisions the page loop makes:
    /// [`Defect::FeedOrdersByTheAcceptanceInstant`] walks the ledger in
    /// another order (through [`MutantLedger::feed_order`], which is the
    /// whole reason that method exists),
    /// [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`] backs the
    /// resumption up by one,
    /// [`Defect::FeedCursorCountsAdmittedEntries`] moves the cursor
    /// assignment inside the admission branch so the cursor stops short,
    /// [`Defect::AFeedPageDropsTheEntryAtItsLimit`] checks the limit after
    /// the cursor has moved so it runs one entry past, and
    /// [`Defect::ABoundedReplayNeverCloses`] keeps minting a continuation
    /// past the `until`. The other four are the decision made *before* the
    /// loop — whether the page is served at all — and they live in
    /// [`MutantLedger::refuses_the_cursor`], which says what each of them
    /// substitutes for the conforming question. Everything else in this
    /// method is a mirror and nothing more — a wrong answer invented for it
    /// would fail a check for a reason no matrix row names.
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
        // retained, so nothing it asks for is missing. Which question the
        // refusal asks of the ledger is the whole of the retention defects;
        // see `MutantLedger::refuses_the_cursor`.
        if matches!(start, FeedStart::After(_))
            && self.refuses_the_cursor(&ledger, subscription, scope, from)
        {
            return Err(UsageCollectorPluginError::CursorBeyondRetention);
        }

        let mut entries = Vec::new();
        let mut cursor = from;
        // `> resumed_at` is the seek.
        // [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`] backs it up by
        // one, which is `>= from` spelled so the comparison itself stays a
        // key comparison rather than becoming an offset.
        let resumed_at = if self.defect == Defect::AFeedPageRedeliversTheEntryAtItsCursor {
            from.saturating_sub(1)
        } else {
            from
        };
        for (key, entry) in self
            .feed_order(&ledger)
            .into_iter()
            .filter(|(key, _)| *key > resumed_at)
        {
            // `upper` names an entry a bounded replay is still asked to read,
            // so the replay stops at the first entry beyond it.
            if key > upper {
                break;
            }
            // A full page stops *before* its cursor moves onto the entry it
            // stopped at, which is what leaves that entry in front of the
            // cursor rather than behind it.
            // [`Defect::AFeedPageDropsTheEntryAtItsLimit`] stops after
            // instead - the row a backend fetched to learn whether a next
            // page exists is the row its cursor ends up naming.
            let full = u64::try_from(entries.len()).unwrap_or(u64::MAX) >= limit;
            if full && self.defect != Defect::AFeedPageDropsTheEntryAtItsLimit {
                break;
            }
            // The cursor moves onto this entry's key whether or not the
            // subscription and the scope admit it. Advancing only past
            // admitted entries would make the position depend on who asked -
            // which is exactly what
            // [`Defect::FeedCursorCountsAdmittedEntries`] does.
            let carried = subscription.contains(&entry.record.gts_type_id)
                && expr_admits(&entry.record, scope);
            if carried || self.defect != Defect::FeedCursorCountsAdmittedEntries {
                cursor = key;
            }
            if full {
                break;
            }
            if carried {
                entries.push(entry.record.clone());
            }
        }

        // [`Defect::ABoundedReplayNeverCloses`] is the whole of the second
        // line: a continuation on every page, the closing page of a bounded
        // replay included.
        let closes =
            until.is_some() && cursor >= upper && self.defect != Defect::ABoundedReplayNeverCloses;
        let next = if closes {
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
            quantity_summary: fold_value(fold, &folded, LatestOrder::of(self.defect))?,
            max_accepted_at: in_scope().map(|entry| entry.accepted_at).max(),
            max_window_end: in_scope().map(|entry| entry.window_end).max(),
        })
    }
}

/// The retention drive, mirroring the reference's.
///
/// Every subject carrying a ledger of its own implements it rather than only
/// the defects that read a mark, for the reason every wrapped subject
/// implements [`ContractRetention`] too: a capability that exists on the shape
/// means any defect routed here can be handed to
/// [`run_all_with_retention`](super::run_all_with_retention) without first
/// growing a second shape. That is not only a convenience — it is what puts a
/// mirror subject in front of the driven assertions at all, which is how the
/// mirror's feed **seek** is pinned. `contract_tests`'
/// `a_subject_carrying_its_own_ledger_is_still_itself_under_a_drive` is the
/// test that spends it, and [`MutantLedger`]'s own docs say what a seek over a
/// ledger with gaps in it establishes that a dense one cannot.
#[async_trait]
impl ContractRetention for MutantLedger {
    /// Runs this subject's retention over one GTS type, to one floor.
    ///
    /// The sweep is [`Ledger::drop_before`]'s, which is the reference's rule:
    /// every entry of the type whose covered period ends before `floor` is
    /// removed and the type's mark rises to the highest sequence removed, both
    /// under one lock acquisition.
    ///
    /// **No defect in this module is about the sweep**, and none should be. A
    /// subject whose sweep removed the wrong rows would be a harness that sets
    /// the wrong scenario up rather than a backend that answers an SPI call
    /// wrongly, and every assertion a drive reaches is about what the backend
    /// *answers* once the rows are gone.
    ///
    /// # Errors
    ///
    /// A message naming the poisoned ledger lock, the one way this can fail.
    async fn drop_before(
        &self,
        gts_type_id: &MeterTypeId,
        floor: time::OffsetDateTime,
    ) -> Result<(), String> {
        let mut ledger = self
            .ledger()
            .map_err(|err| format!("the mutant ledger could not run retention: {err}"))?;
        ledger.drop_before(gts_type_id, floor);
        Ok(())
    }
}

/// The subject one defect names, as a concrete ledger-carrying type a driven
/// run can hold.
///
/// [`drivable_mutant`] is this for the wrapped shape and says why the erasure
/// [`mutant`] applies is useless to
/// [`run_all_with_retention`](super::run_all_with_retention): that entry point
/// wants the same backend as a [`ContractRetention`] too, and a value erased
/// behind one of the two traits cannot be recovered as the other.
///
/// **Both shapes are now offered**, where only the wrapped one used to be. The
/// four retention-refusal defects live inside `read_feed_page`'s own refusal
/// branch, and a wrapper cannot reach it: the branch is the inner backend's,
/// the inner backend refuses correctly, and no interception can make a
/// conforming backend fail to refuse. They carry a ledger of their own for the
/// same reason the five feed defects before them do.
pub(super) fn drivable_ledger_mutant(defect: Defect) -> MutantLedger {
    MutantLedger::new(defect)
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

/// [`Defect::ADivergentWriteDisplacesTheSurvivor`]: decides one entry against
/// the ledger exactly as [`admit`] decides it, and writes a submission it
/// refused over the row it refused it against.
///
/// The answer is the conforming one in all three cases - the stored entry for
/// an exact retry, `IdempotencyConflict` carrying it for a divergent
/// submission, the entry itself for a fresh one - because it is [`decide`]'s,
/// the same function [`admit`] consults, and it is taken *before* the write.
/// Only the store differs: the conflicting submission replaces the row it
/// collided with instead of being dropped, which is `ON CONFLICT (the six
/// identity columns) DO UPDATE` where the conforming statement is
/// `DO NOTHING`.
///
/// An identical re-delivery writes nothing, which is what keeps this subject
/// wrong in one way only. Absorbing writes the same content back and could
/// not be observed; refusing to write it is the conforming behaviour and the
/// exemplar's, so the divergent branch is the whole of the defect.
fn admit_displacing_the_survivor(
    ledger: &mut Ledger,
    record: UsageRecord,
) -> Result<UsageRecord, UsageCollectorPluginError> {
    match decide(ledger, &record) {
        Ok(Some(stored)) => Ok(stored),
        Ok(None) => {
            ledger.push(record.clone());
            Ok(record)
        }
        Err(err) => {
            ledger.replace(record);
            Err(err)
        }
    }
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
    latest_order: LatestOrder,
) -> Result<AggregationResult, UsageCollectorPluginError> {
    if group_by.is_empty() {
        return Ok(AggregationResult {
            buckets: vec![AggregationBucket {
                key: Vec::new(),
                value: fold_value(fold, rows, latest_order)?,
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
            value: fold_value(fold, &group, latest_order)?,
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
/// **`LATEST` takes whichever order [`LatestOrder`] names**, which is
/// [`LatestOrder::Declared`] — the whole of DESIGN §3.1's three keys — for
/// every subject but the three that exist to get one of those keys wrong.
/// Under the declared order all three keys are read, and `id` is compared as
/// bytes by [`Uuid`]'s derived `Ord`, so the order is total and the answer
/// never depends on ledger insertion order.
fn fold_value(
    fold: AggregationFold,
    rows: &[&UsageRecord],
    latest_order: LatestOrder,
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
        AggregationFold::Latest => latest_order.pick(rows),
        AggregationFold::Count => None,
    };
    winner
        .map(|row| widen(row.quantity.as_decimal()))
        .transpose()
}

/// Which of DESIGN §3.1's three `LATEST` keys a subject's fold reads.
///
/// The order is *"Greatest `window_end`, then greatest `accepted_at`, then
/// greatest `id` in byte order"*, and one variant here strikes out each of
/// the three. They are spelled as one enum rather than as three branches on
/// [`Defect`] because that is what makes the set visibly complete: a fourth
/// way to get this order wrong would be a fourth key, and there is no fourth
/// key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LatestOrder {
    /// All three keys, as DESIGN declares them. Every subject but three.
    Declared,
    /// `(window_end, id)` — [`Defect::LatestSkipsTheAcceptanceInstant`].
    WithoutTheAcceptanceInstant,
    /// `(window_end, accepted_at)` —
    /// [`Defect::LatestStopsAtTheAcceptanceInstant`]. What is left is settled
    /// on arrival, because [`Iterator::max_by_key`] answers the **last**
    /// maximum: a partial order does not stop a backend answering, it stops
    /// the answer meaning anything.
    WithoutTheIdentifier,
    /// `(accepted_at, id)` — [`Defect::LatestIgnoresThePeriodEnd`].
    WithoutThePeriodEnd,
}

impl LatestOrder {
    /// The order one subject folds `LATEST` under.
    fn of(defect: Defect) -> Self {
        match defect {
            Defect::LatestSkipsTheAcceptanceInstant => Self::WithoutTheAcceptanceInstant,
            Defect::LatestStopsAtTheAcceptanceInstant => Self::WithoutTheIdentifier,
            Defect::LatestIgnoresThePeriodEnd => Self::WithoutThePeriodEnd,
            // Enumerated rather than caught by a wildcard, for the reason
            // `WrappedReference::admit_here` enumerates its own: a fold
            // defect added later and forgotten here would silently fold
            // under DESIGN's order and report nothing at all, which is a
            // matrix row whose subject does nothing rather than a compile
            // error. The wrapped subjects never reach this type - `mutant`
            // routes them to `WrappedReference` - but exhaustiveness is the
            // point.
            Defect::QuantityThroughFloat
            | Defect::StampsItsOwnAcceptedAt
            | Defect::DefaultsOriginToLive
            | Defect::SelectsOnWindowStart
            | Defect::DedupIgnoresThePeriod
            | Defect::DedupIgnoresTheEntryType
            | Defect::ConflictReadBackIgnoresTheEntryType
            | Defect::FoldsTheInvalidation
            | Defect::EmptySumIsAbsent
            | Defect::CoalescesEveryEmptyFoldToZero
            | Defect::AGroupNothingSurvivesInStillGetsABucket
            | Defect::AbsorbsAWithdrawalWithAnotherReason
            | Defect::RefusesAWithdrawalWithTheSameReason
            | Defect::ConflictNamesTheRecord
            | Defect::IgnoresScopeOnThePointRead
            | Defect::AnswersNotConvergedForAnAcknowledgedEntry
            | Defect::LedgerHasNoUniqueConstraint
            | Defect::BatchResolvesAgainstThePreCallLedger
            | Defect::ADivergentWriteDisplacesTheSurvivor
            | Defect::FeedCursorCountsAdmittedEntries
            | Defect::AFeedPageRedeliversTheEntryAtItsCursor
            | Defect::ABoundedReplayNeverCloses
            | Defect::AFeedPageDropsTheEntryAtItsLimit
            | Defect::AFeedBootstrapReadStartsAtTheHead
            | Defect::RefusesTheOldestStartAfterASweep
            | Defect::SkipsTheOldestEntryASweepLeft
            | Defect::ServesAShortPageWhereRetentionTruncatedACursor
            | Defect::TheRetentionRefusalReadsTheCallersGrant
            | Defect::RefusesEveryCursorOnceASweepHasRun
            | Defect::TheRetentionRefusalIgnoresTheSubscription
            | Defect::AFeedPositionIsKeyedPerTenant
            | Defect::AFeedPositionIsKeyedPerSubscribedType
            | Defect::FeedOrdersByTheAcceptanceInstant => Self::Declared,
        }
    }

    /// The row this order picks out of a bucket, or `None` when the bucket is
    /// empty.
    fn pick<'rows>(self, rows: &'rows [&UsageRecord]) -> Option<&'rows &'rows UsageRecord> {
        match self {
            Self::Declared => rows
                .iter()
                .max_by_key(|row| (row.window_end, row.accepted_at, row.id)),
            Self::WithoutTheAcceptanceInstant => {
                rows.iter().max_by_key(|row| (row.window_end, row.id))
            }
            Self::WithoutTheIdentifier => rows
                .iter()
                .max_by_key(|row| (row.window_end, row.accepted_at)),
            Self::WithoutThePeriodEnd => rows.iter().max_by_key(|row| (row.accepted_at, row.id)),
        }
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
