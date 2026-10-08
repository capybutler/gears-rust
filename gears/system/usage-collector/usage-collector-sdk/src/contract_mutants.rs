//! Deliberately non-conforming backends, one per rule the suite checks.
//!
//! A green `the_reference_backend_conforms` establishes that the suite *runs*,
//! not that any check would notice a non-conforming plugin — and a check that
//! cannot fail reads as coverage while accepting a broken port. Each subject
//! here is **behaviourally** the reference backend wrong in exactly one
//! plausible way, and `each_check_fails_against_its_own_defect_and_no_other`
//! asserts a full column against each of them.
//!
//! Nothing here lives in [`super::reference`]: that is the one worked
//! implementation of this SPI, and a defect switch inside it would put
//! deliberate wrongness in the exemplar.
//!
//! # Two shapes
//!
//! **A wrapper** ([`WrappedReference`]) delegates to a real
//! [`InMemoryReferencePlugin`] and intercepts one method, so everything the
//! defect is not about is the exemplar's own behaviour. Used wherever the
//! defect is a function of what the exemplar already computed: a rewritten
//! field on the way in, an index kept beside the ledger, what the point read
//! answers, how a second withdrawal is answered, what `FeedStart::Oldest`
//! names, how a position is re-encoded, what a fold answers over an empty
//! selection, and the reconciliation summary's branch.
//!
//! **A ledger of its own** ([`MutantLedger`]) is needed where the defect
//! changes something the inner backend owns and no interception can reach:
//! which column a range meets, which rows a fold walks, what admission writes,
//! what a batch is decided against, which row a fold ranks highest, in what
//! order a feed page walks, where it resumes, how far its cursor advances,
//! whether a bounded replay closes, what the retention refusal asks of its own
//! marks, and what a raw page's continuation counts past. It mirrors the
//! reference elsewhere, and is smaller in one stated way no check reaches —
//! see [`MutantLedger`].
//!
//! [`carries_its_own_ledger`] is which is which.
//!
//! # The feed defects
//!
//! `read_feed_page` is one loop making four decisions, each with a subject,
//! all routed to [`MutantLedger`]: the walk order
//! ([`Defect::FeedOrdersByTheAcceptanceInstant`]), where it resumes
//! ([`Defect::AFeedPageRedeliversTheEntryAtItsCursor`]), how far the cursor
//! advances ([`Defect::FeedCursorCountsAdmittedEntries`] stops it short and
//! [`Defect::AFeedPageDropsTheEntryAtItsLimit`] runs it one entry past), and
//! whether it closes ([`Defect::ABoundedReplayNeverCloses`]).
//!
//! **Whether the page is served at all** is decided above the loop, against
//! DESIGN §3.3's `feed-retention-refusal` row: the refusal reads what a sweep
//! removed and nothing else. Four subjects break it —
//! [`Defect::ServesAShortPageWhereRetentionTruncatedACursor`] asks nothing,
//! [`Defect::TheRetentionRefusalReadsTheCallersGrant`] asks what the caller's
//! grant carried, [`Defect::RefusesEveryCursorOnceASweepHasRun`] asks only
//! whether a sweep ran, and
//! [`Defect::TheRetentionRefusalIgnoresTheSubscription`] asks it of every type
//! at once. All four show nothing until a sweep has run, so they are reached
//! through [`drivable_ledger_mutant`] and [`super::run_all_with_retention`];
//! `contract_tests`' `RETENTION_DRIVEN_MATRIX` asserts their columns.
//!
//! **What `start` names** is decided before the loop, against
//! `feed-bootstrap-position`'s clauses, and every subject for them is a
//! wrapper: never the head ([`Defect::AFeedBootstrapReadStartsAtTheHead`]),
//! never refused on the retention floor
//! ([`Defect::RefusesTheOldestStartAfterASweep`]), and at the oldest entry the
//! subscription retains ([`Defect::SkipsTheOldestEntryASweepLeft`]). The last
//! two are reached through [`drivable_mutant`].
//!
//! **How a position is encoded** is decided after the loop. §3.1 leaves a
//! position's structure to the plugin and takes its encoded size back — it
//! *"may not grow with a subscription's breadth"*.
//! [`Defect::AFeedPositionIsKeyedPerTenant`] appends one component per tenant,
//! [`Defect::AFeedPositionIsKeyedPerSubscribedType`] one per type. Both strip
//! their components on the way in, so each resumes exactly where the
//! exemplar's would and only the size is wrong.
//!
//! # DESIGN's empty-selection clauses
//!
//! §3.3: *"`SUM` and `COUNT` are defined over an empty selection and report
//! `0`; `MAX`, `MIN` and `LATEST` are not and report absent. […] A grouped
//! query yields no bucket for a group nothing survives in."* One wrapper each:
//! [`Defect::EmptySumIsAbsent`] answers absent, as SQL `SUM(x)` does;
//! [`Defect::CoalescesEveryEmptyFoldToZero`] answers `0`; and
//! [`Defect::AGroupNothingSurvivesInStillGetsABucket`] yields a bucket. The
//! first two are each other's mirror on purpose: the obligation is a split, so
//! a check asserting one side alone is passed by a backend that collapses it
//! the other way.
//!
//! **`COUNT` over an empty selection has no subject of its own** — a recorded
//! gap. `EmptySumIsAbsent` breaks the clause the two folds share, so
//! `invalidation-excluded-from-fold`'s `COUNT` assertion is reached by no
//! subject.
//!
//! # DESIGN's `LATEST` keys
//!
//! §3.1 orders by *"Greatest `window_end`, then greatest `accepted_at`, then
//! greatest `id` in byte order"*, and one subject strikes out each:
//! [`Defect::LatestIgnoresThePeriodEnd`],
//! [`Defect::LatestSkipsTheAcceptanceInstant`] and
//! [`Defect::LatestStopsAtTheAcceptanceInstant`]. The set is complete because
//! the order is. [`LatestOrder`] carries one variant per key omitted.
//!
//! # DESIGN's identity sites
//!
//! §3.3: *"Everything keyed on identity — a unique constraint or conflict
//! target, the read-back of a conflicting entry, an in-batch dedup map —
//! includes `entry_type` or keys on `id`, which covers all six inputs."*
//!
//! * **Unique constraint** — [`Defect::DedupIgnoresTheEntryType`] keys on five
//!   of six; [`Defect::LedgerHasNoUniqueConstraint`] omits it altogether.
//! * **Conflict read-back** — [`Defect::ConflictReadBackIgnoresTheEntryType`].
//! * **In-batch dedup map** —
//!   [`Defect::BatchResolvesAgainstThePreCallLedger`], which has none.
//!
//! # The server-assigned fields
//!
//! `server-field-round-trip` asserts `id`, `accepted_at`, `origin` and an
//! invalidation's `invalidates`. Only `accepted_at`
//! ([`Defect::StampsItsOwnAcceptedAt`]) and `origin`
//! ([`Defect::DefaultsOriginToLive`], whose fixtures hand the plugin both
//! origins so a backend answering one always is caught) have a subject, which
//! bounds what that check's matrix row establishes. `id` wants none — the
//! derivation is a deterministic `UUIDv5` over the identity inputs, so
//! re-deriving is a no-op. `invalidates` is a known gap: a backend dropping the
//! reference on read also breaks the withdrawal exclusion
//! ([`withdrawn_targets`] reads exactly that field), so the defect would land
//! as a multi-check row rather than an isolating one.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use toolkit_odata::{ODataQuery, ast};
use uuid::Uuid;

use super::reference::{
    InMemoryReferencePlugin, order_is_ascending, page_with_continuation, seek_past_boundary,
    sort_by_dispatched_order,
};
use super::retention::ContractRetention;
use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPage, FeedPosition, FeedStart};
use crate::keyset::{Keyset, RecordPage};
use crate::models::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult,
    MAX_AGGREGATION_BUCKETS, MetadataFilter, RecordOrigin,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::reconciliation::{ObservedQuantity, QuantitySummary, ReconciliationMetadata};
use crate::stored::{MeterRef, StoredUsageRecord};
use crate::time_range::TimeRange;

/// A backend that is the reference backend with one rule broken.
///
/// One rule each, deliberately: a mutant wrong in two ways fails two checks
/// and proves neither of them was the one that noticed.
///
/// Each defect is the *plausible* wrong implementation, not an absurd one. A
/// backend that returns garbage is caught by anything; the question is whether
/// the suite catches the mistake someone would actually make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Defect {
    /// Stores the quantity through an `f64` — the mistake a backend makes by
    /// choosing a `double precision` column.
    QuantityThroughFloat,
    /// Stamps its own insert time into `accepted_at`, discarding the instant
    /// it was handed — the mistake a backend makes by declaring the column
    /// `DEFAULT now()` and leaving it out of the insert.
    ///
    /// DESIGN §3.1's "Server-assigned field fidelity" names this one outright:
    /// a plugin *"does not re-derive, default, or refresh one — `accepted_at`
    /// in particular is not the store's own insert time."*
    ///
    /// Applied on admission, so every read path answers the substituted instant
    /// and so does the absorbed retry. What that costs the matrix is stated at
    /// [`MUTANT_INSERT_INSTANT`].
    StampsItsOwnAcceptedAt,
    /// Writes `live` into `origin` whatever it was handed — the mistake a
    /// backend makes by declaring the column `DEFAULT 'live'` and leaving it
    /// out of the insert, or by hard-coding the live route in a port written
    /// before the backfill one existed.
    ///
    /// DESIGN §3.1's "Server-assigned field fidelity" names this one too: a
    /// plugin *"does not re-derive, default, or refresh one"*. `origin` is the
    /// field of the four a default is most natural on, because one of its two
    /// values is overwhelmingly the common case.
    ///
    /// Applied on admission, like [`Self::StampsItsOwnAcceptedAt`]: a defaulted
    /// column is written once, on the way in.
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
    /// The second of DESIGN §3.3's identity sites (see this module's
    /// header). A backend can get the unique constraint right and this wrong:
    /// the `INSERT … ON CONFLICT (six columns) DO NOTHING` names all six, while
    /// the `SELECT` that follows it is written by hand against the idempotency
    /// key and its covered period.
    ///
    /// Such a read-back can see two rows, and this subject takes the later one
    /// — structural rather than an arbitrary tie-break, since an invalidation
    /// names a target that must already be stored.
    ConflictReadBackIgnoresTheEntryType,
    /// Excludes the withdrawn record from the fold but folds the
    /// invalidation. DESIGN names this one: it double-counts the withdrawn
    /// measurement.
    FoldsTheInvalidation,
    /// Answers absent for `SUM` over an empty selection, where DESIGN defines
    /// it as `0`.
    ///
    /// It is `SUM`'s own answer in SQL — `SUM(x)` over no rows is `NULL` — so a
    /// backend gets this wrong by writing the obvious expression and nothing
    /// else, which is why the subject exists even though the rule it breaks is
    /// a single clause.
    EmptySumIsAbsent,
    /// Answers `0` for `MAX`, `MIN` and `LATEST` over an empty selection,
    /// where DESIGN has all three report absent — the mistake a backend
    /// makes by wrapping every fold in `COALESCE(…, 0)` once it has found
    /// out that `SUM` needs one.
    ///
    /// [`Self::EmptySumIsAbsent`]'s mirror: DESIGN §3.3's obligation is a
    /// **split**, so one subject collapses it each way and a check asserting
    /// only one side would pass one of them.
    CoalescesEveryEmptyFoldToZero,
    /// Emits a bucket for a group nothing survives in, keyed from the rows
    /// the range selects rather than from the rows that survive the fold —
    /// the mistake a backend makes by leaving the withdrawal exclusion out
    /// of its `WHERE` and putting it in a `FILTER (WHERE …)` on the
    /// aggregate instead.
    ///
    /// That rewrite is exact under every fold **but** for which groups exist:
    /// `GROUP BY` forms a group per key the range holds, so the group whose
    /// every row the filter removes comes back as a bucket. DESIGN §3.3 ends on
    /// that case — *"A grouped query yields no bucket for a group nothing
    /// survives in"* — and it runs the opposite way from the ungrouped clause,
    /// which is why neither [`Self::EmptySumIsAbsent`] nor
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
    /// The mistake a backend makes by reading "at most one invalidation" as a
    /// rule about the **record**: the collision is detected on the right
    /// identity, and the entry handed back is then looked up through
    /// `invalidates` rather than taken from the identity that collided. DESIGN
    /// §3.3 rules that out — the `existing` is *"that invalidation, never the
    /// record"* — and a caller told its withdrawal collided with the
    /// measurement learns nothing it can act on, since the gateway lifts the
    /// conflict to `AlreadyInvalidated` naming a reason code the record does
    /// not carry.
    ///
    /// **Wrong about the answer alone**, and only on a refusal raised for an
    /// entry carrying an invalidation, which is why it can be a wrapper. Those
    /// two confinements keep it to one check even though two others compare a
    /// conflict's `existing`: `dedup-floor` submits no invalidation at all, and
    /// `record-and-invalidation-distinct-identity` has all three of its
    /// submissions accepted, so neither raises a refusal to rewrite.
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
    /// answers `UsageRecordNotConverged` only until it can decide"*.
    ///
    /// It is the mistake a backend makes by reading `converged_only` as "say so
    /// when you are not sure": a port that routes point reads to a replica
    /// pool, cannot establish that the pool has caught up, and declines to
    /// decide rather than declining to lag.
    ///
    /// **Applied only where the inner backend answered `Ok`**, so the subject
    /// is wrong about convergence rather than about absence. Reaches
    /// `converged-target-lookup`'s first probe and nothing else — no other check
    /// dispatches a lookup with `converged_only = true` — and is the only
    /// subject driving the `Eventual` half of it, since the matrix runs every
    /// subject at `Linearizable` where an undecided answer is a violation
    /// outright. See
    /// `an_undecided_lookup_is_still_a_violation_once_the_bound_has_passed`.
    AnswersNotConvergedForAnAcknowledgedEntry,
    /// Decides every collision by reading the ledger and then writes anyway:
    /// the dedup identity carries no unique constraint, so the outcome a
    /// caller reads is right and a second row lands beside the first.
    ///
    /// The mistake a backend makes by leaving the dedup logic in the
    /// application and the constraint out of the schema: the `SELECT` deciding
    /// absorb-or-conflict is written, the `INSERT` after it is unconditional,
    /// and nothing in the table refuses the duplicate. DESIGN §3.1's "Dedup
    /// identity" row states the floor it breaks: *"One identity yields at most
    /// one entry on every read path, fold, reconciliation figure, and
    /// materialised aggregate."*
    ///
    /// Every collision outcome it returns is the conforming one, so what it
    /// fails is the read-back — which is the point: a subject wrong in its
    /// *answers* would fail the absorb and conflict assertions and leave the
    /// floor itself untested. It reaches exactly `dedup-floor`'s two
    /// ledger-reading assertions, the row count on `list_usage_records` and the
    /// `COUNT` fold over the same range.
    ///
    /// **`server-field-round-trip` passes this subject on two orderings rather
    /// than anything structural**, which the next author to touch that check
    /// should know: the duplicate does not exist until the fourth of its five
    /// properties has run, and the fifth looks its entry up by `id` through
    /// `get_usage_record`, which answers the first match while
    /// [`Ledger::records`] is in admission order.
    LedgerHasNoUniqueConstraint,
    /// Resolves every entry of a batch against the ledger **as it stood
    /// before the call**, so two same-identity entries in one
    /// `create_usage_records` are both reported accepted instead of the later
    /// resolving against the earlier.
    ///
    /// **The last of DESIGN §3.3's identity sites** (see this module's header),
    /// and strictly the site **missing** rather than mis-keyed — the rule is on
    /// [`create_usage_records`](crate::plugin_api::UsageCollectorPluginV1::create_usage_records).
    /// A backend that reads its dedup state once, decides every row against
    /// that read, and then writes them all makes exactly this mistake.
    ///
    /// **The write still dedups**, which keeps the subject wrong in one place
    /// rather than two — [`Self::LedgerHasNoUniqueConstraint`] already strikes
    /// out the unique constraint — so the damage is confined to the outcome the
    /// caller is handed: an acceptance reported for a row never written.
    BatchResolvesAgainstThePreCallLedger,
    /// Answers every collision the conforming way and then writes the
    /// divergent submission over the row it just refused: last writer wins
    /// on the store even though the caller was told otherwise.
    ///
    /// DESIGN §3.1's "Dedup level" row is what this breaks: *"The first write
    /// in commit order is the **survivor**, and every read path, fold,
    /// reconciliation figure, materialised aggregate, and the feed show it and
    /// nothing else"*. It keeps the companion clause — no outcome it returns
    /// accepts divergent content.
    ///
    /// It is the mistake a backend makes by writing `INSERT … ON CONFLICT (the
    /// identity columns) DO UPDATE SET …` where the conforming statement is
    /// `DO NOTHING`, and then deciding absorb-or-conflict from the row
    /// `RETURNING` handed back: the decision is right, the store is wrong. A
    /// port written against an upsert-shaped table arrives here naturally.
    /// **Distinct from [`Self::AbsorbsAWithdrawalWithAnotherReason`]**, which
    /// changes the *answer* and no row, only for invalidations.
    ///
    /// Reaches one assertion in one check — `dedup-concurrent`'s third probe,
    /// which reads a raced identity's content back. Nothing else reads content
    /// back after a divergent submission, and the check's `Eventual` half
    /// cannot see it either: the row is replaced in place, so every read path
    /// still shows one write per identity and no `eventual` outcome names the
    /// survivor. The matrix runs every subject at `Linearizable`, where they
    /// do.
    ADivergentWriteDisplacesTheSurvivor,
    /// Folds `LATEST` on `(window_end, id)`, the middle key of DESIGN §3.1's
    /// three struck out.
    ///
    /// The two keys it keeps are the two a backend already has an index on: a
    /// ledger page is ordered by `(window_end, id)` throughout this SPI. The
    /// order it yields is still *total*, so nothing about the answer looks
    /// unreliable — it is simply the wrong entry whenever the middle key and
    /// the last disagree.
    ///
    /// **It reaches `latest-tie-break`'s `accepted_at` scenario and no
    /// other.** The `window_end` scenario separates on the first key, which
    /// this subject keeps; the cross-tenant scenario ties on the first two and
    /// falls to `id`, which it also keeps and which is DESIGN's own answer
    /// there.
    LatestSkipsTheAcceptanceInstant,
    /// Folds `LATEST` on `(window_end, accepted_at)` and settles what is left
    /// on arrival order - the last key of DESIGN §3.1's three struck out, so
    /// the order is no longer total.
    ///
    /// The mistake is a **missing** key rather than a substituted one, and
    /// DESIGN states the consequence: `id` *"is unique, so the order is
    /// total"*. A backend whose `ORDER BY` stops at `accepted_at` answers
    /// whichever row its scan reached last, so two entries a deployment cannot
    /// tell apart get an answer that depends on the storage layout — the class
    /// of mistake a counter monotonic per `(tenant_id, gts_type_id)` alone
    /// makes, ranking something inside one tenant and nothing across a group
    /// spanning tenants.
    ///
    /// **It reaches `latest-tie-break`'s cross-tenant scenario and no other**,
    /// and is the only subject that reaches it: the other two keep `id` and so
    /// agree with DESIGN wherever the two keys above it tie. That scenario
    /// submits its two entries in descending `id` order precisely so this
    /// subject's arrival tie-break picks the loser.
    LatestStopsAtTheAcceptanceInstant,
    /// Folds `LATEST` on `(accepted_at, id)`, the **first** key of DESIGN
    /// §3.1's three struck out.
    ///
    /// *Latest* reads as *most recently accepted*, and a backend that takes the
    /// fold's name at its word orders by the acceptance instant. It is also
    /// where a port of a pre-period model lands: with no period column to order
    /// on, the acceptance instant is the only time a row carries. DESIGN puts
    /// the period end first because an entry states *when the measurement
    /// covers*, not when the gear happened to accept it.
    ///
    /// **It reaches `latest-tie-break`'s `window_end` scenario and no other.**
    /// That scenario is built for it: its later-ending entry carries the
    /// *smaller* acceptance instant, so a fold that reads `accepted_at` first
    /// reports the other entry rather than agreeing by accident.
    LatestIgnoresThePeriodEnd,
    /// Advances a feed page's cursor past every entry the page **carried**
    /// rather than past every entry it **scanned** — the scope and the
    /// subscription gate moved above the cursor assignment.
    ///
    /// **It is a one-line change**, which is what makes it plausible: writing
    /// the cursor inside the `if` that already decides whether to carry the row
    /// reads as tidier, and it is correct under every single grant.
    ///
    /// What it breaks is the position's **meaning**. DESIGN §3.1 fixes a
    /// position's age by the oldest subsequent entry of a subscribed type
    /// *"whether or not the reader's authorization scope admits that entry"*,
    /// and states that *"A page reaching the settled head returns its cursor at
    /// the head"*. Under this defect a position means "the last entry this
    /// grant admitted", so a cursor minted under one grant and resumed under a
    /// wider one silently skips every entry the narrower grant withheld.
    ///
    /// **The suite reaches it from one direction only** —
    /// `a_wider_grant_resumes_a_narrower_walk_at_the_head`. The position is an
    /// entry's *sequence* rather than a count, so nothing is re-delivered and
    /// no single-grant walk observes anything amiss; the cursor merely stops
    /// short, which only a reader whose grant admits more can see.
    FeedCursorCountsAdmittedEntries,
    /// Resumes a feed page at the entry a position names instead of after
    /// it — `WHERE sequence >= :cursor` where the contract wants
    /// `> :cursor`.
    ///
    /// **The classic keyset off-by-one**, and the one every paginated read path
    /// is one character away from: DESIGN §3.3 forbids offset/limit scans on
    /// both paginated paths, so the resumption has to be a key comparison —
    /// which is exactly where the inclusive/exclusive choice is got wrong.
    ///
    /// What it costs is a **stall**: every page after the first re-delivers the
    /// entry its own start position named and, at a page limit of one, carries
    /// nothing else. Two of DESIGN's clauses fail at once — the entry repeats,
    /// and the entries after it never arrive.
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
    /// for the second, the one a backend is likeliest to miss because minting
    /// a cursor is what every other page does.
    ///
    /// Nothing else about the replay changes. What is lost is the **signal**:
    /// an absent `next` is the only thing that tells a caller a bounded replay
    /// is finished, so one that keeps minting a continuation leaves a consumer
    /// following a cursor over a range it has already read whole.
    ABoundedReplayNeverCloses,
    /// Advances a feed page's cursor onto the entry the page stopped at, so
    /// that entry ends up **behind** the cursor instead of in front of it —
    /// the page limit checked one statement too late.
    ///
    /// **The classic short-page off-by-one, and the only feed defect here that
    /// loses an entry outright.** A backend telling a caller whether a next
    /// page exists fetches `limit + 1` rows and returns `limit` of them; the
    /// cursor it mints has to come from the last row it **returned**, never
    /// from the last row it **fetched**. Taking it from the last row fetched is
    /// one subscript, and it reads correctly under every limit the ledger never
    /// reaches. Spelled as a loop, as here, it is the `break` moved below the
    /// cursor assignment rather than above it.
    ///
    /// What it breaks is the clause of DESIGN §3.1's Feed order invariant the
    /// other feed subjects leave alone: *"no entry the read's compiled scope
    /// admits ever becomes visible behind a returned cursor, whatever the
    /// concurrency or commit order."* The entry it skips is settled, inside the
    /// subscription and the scope, and behind a cursor its consumer already
    /// holds: nothing delivers it again.
    ///
    /// **It reaches `feed-completeness` alone.** It misses
    /// `feed-snapshot-and-replay` through that check's fixtures rather than its
    /// assertions: its limit-of-one walk alternates an admitted tenant with a
    /// withheld one, so the entry every page skips is one the walking grant was
    /// never going to carry, and its wider reads run at a limit its ledger
    /// never reaches. Worth knowing before either check's ledger is edited: a
    /// row growing here would not be a second mistake.
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
    /// It is the mistake a backend makes by having no feed-order column at all.
    /// `accepted_at` is already stored, already indexed for the reconciliation
    /// watermarks, already monotonic in every single-writer test, and the only
    /// field on a [`StoredUsageRecord`] that looks like an arrival. A port that
    /// reaches for it is wrong only when two writers disagree about the clock —
    /// exactly when a correction can be stamped before the thing it corrects.
    ///
    /// **The key stays unique and the order stays total**, so nothing about
    /// this subject looks unreliable; what moves is where an invalidation sits
    /// relative to its target.
    ///
    /// **It reaches `feed-completeness`'s correction-order assertion alone**,
    /// the only one in the suite it could reach: every other feed read is over
    /// entries sharing one acceptance instant, or finds what it wants by `id`
    /// rather than by position.
    FeedOrdersByTheAcceptanceInstant,
    /// Begins a bootstrap feed read at the head — `FeedStart::Oldest`
    /// delegated as `FeedStart::After(<the position the feed has reached>)`.
    ///
    /// **The mistake a backend makes by treating "no cursor supplied" as
    /// "start from now".** It is the natural shape when a feed is built on a
    /// change stream, a logical-replication slot or a `LISTEN`/`NOTIFY`
    /// channel, all of which hand out a position at the moment you subscribe. A
    /// port that reaches for it answers every resumed read correctly and then
    /// hands every *new* consumer an empty stream and a cursor at the head.
    /// DESIGN rules it out where it defines the name: *"`FeedStart::Oldest`
    /// means the oldest position this plugin still serves for `subscription`
    /// under `scope` — never the head"*.
    ///
    /// **A wrapper**, because it lands in how `start` is *interpreted* before
    /// the loop runs. The head position it substitutes is read out of the inner
    /// backend — one unbounded page over the same subscription and scope, whose
    /// cursor is by definition the head. Nothing else changes:
    /// `FeedStart::After` is delegated untouched.
    AFeedBootstrapReadStartsAtTheHead,
    /// Refuses `FeedStart::Oldest` with `CursorBeyondRetention` once
    /// retention has swept a subscribed type — the cursor refusal applied to
    /// both arms of `FeedStart` instead of to one.
    ///
    /// **The mistake a backend makes by deciding the refusal before it looks
    /// at where the read begins.** `read_feed_page` has one retention check to
    /// make and two start modes to make it for. Written once, above the branch,
    /// it is correct for `After` and wrong for `Oldest` — which asks for no
    /// particular continuation and so cannot have lost one. DESIGN §3.3's
    /// `feed-bootstrap-position` row: `FeedStart::Oldest` *"is never refused on
    /// the retention floor"*. It costs every consumer that bootstraps on a
    /// backend old enough to have swept once, permanently, since the mark only
    /// ever rises.
    ///
    /// **Not in the discrimination matrix, and could not be:** the defect needs
    /// a mark, which needs a sweep, which needs a driver `super::run_all` does
    /// not hand. It is a row of `contract_tests`' `RETENTION_DRIVEN_MATRIX`
    /// instead.
    ///
    /// The mark it keeps is its own, since a wrapper cannot see the inner
    /// backend's. It records the type of every drop driven through it, which
    /// over-approximates a real mark — but only in the direction that makes the
    /// subject *more* wrong, so it cannot hide the defect it exists to show.
    RefusesTheOldestStartAfterASweep,
    /// Begins a bootstrap feed read one entry **past** the oldest entry a
    /// sweep left — the bootstrap position derived from the retention floor
    /// with the boundary the wrong side of it.
    ///
    /// **The mistake a backend makes by resolving `FeedStart::Oldest` out of
    /// its own retention marks.** A mark records the highest position of the
    /// entries a sweep removed, so "the oldest I still serve" is *after* the
    /// mark — correct only while the boundary is exclusive on the mark and
    /// inclusive on the first row above it. That is the same
    /// inclusive/exclusive choice
    /// [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`] gets wrong on the
    /// resume path, here got wrong in the other direction: one entry is skipped
    /// rather than repeated, silently, per sweep.
    ///
    /// It covers the half of `feed-bootstrap-position` that
    /// [`Defect::RefusesTheOldestStartAfterASweep`] does not — the row states
    /// both *"begins at the oldest entry the subscription retains … and is
    /// never refused on the retention floor"*.
    ///
    /// The position it skips to is read out of the inner backend (one page at a
    /// limit of one from `Oldest`) for want of any way for a wrapper to see a
    /// sequence; a backend making this mistake for real reads it from its marks
    /// table.
    ///
    /// **Nothing shows until a sweep has run, so it is not in the
    /// discrimination matrix**, for the reason
    /// [`Defect::RefusesTheOldestStartAfterASweep`] is not.
    SkipsTheOldestEntryASweepLeft,
    /// Serves a cursor whose continuation retention has truncated as an
    /// ordinary page — the refusal left out altogether.
    ///
    /// **The failure DESIGN §3.3's `feed-retention-refusal` row names
    /// outright**: a cursor after which retention has removed an entry of a
    /// subscribed GTS type *"is refused rather than served as a short page"*.
    /// It is the mistake a backend makes by having no marks table at all — the
    /// sweep drops the rows, the feed goes on seeking past whatever position it
    /// is handed, and every read after a sweep is answered correctly *except*
    /// the ones that span it. A port arrives here by building the feed first
    /// and the retention interlock second.
    ///
    /// It is the only feed failure a consumer cannot see: the page is well
    /// formed, its cursor advances, and the swept entries are simply never
    /// delivered. Every other way a feed can go wrong refuses, repeats, or
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
    /// leaves a gap no feed error marks.
    ///
    /// This subject keeps the removed entries rather than only their highest
    /// sequence, which is what lets it evaluate the caller's scope against
    /// them. A real backend with a per-tenant marks table needs no such memory.
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
    /// permitted to remove*. They are not the same set, because §3.1's
    /// "Plugin-owned lifecycle" row makes the horizon a floor rather than a
    /// boundary — *"A purge later than the horizon is permitted"*.
    ///
    /// It is the mistake a backend makes by storing the instant it last swept
    /// to and refusing any cursor issued before it, or by testing
    /// `mark IS NOT NULL` where the contract wants `mark > :position`.
    ///
    /// What it costs is every consumer at once, and permanently: a retention
    /// mark only ever rises, so from the first sweep onwards no consumer can
    /// resume and none can retry its way out. It is also the easiest to miss in
    /// review, because the refusal it returns is the contract's own variant.
    ///
    /// **Nothing shows until a sweep has run**, for the reason
    /// [`Self::ServesAShortPageWhereRetentionTruncatedACursor`] gives.
    RefusesEveryCursorOnceASweepHasRun,
    /// Reads its retention marks across every GTS type at once rather than
    /// per subscribed type, so a sweep over one meter refuses a cursor over
    /// another.
    ///
    /// `usage-collector-v1.yaml` states the obligation it breaks: removal *"is
    /// read per subscribed GTS type"*, and DESIGN §3.1 fixes a position's age
    /// by *"the oldest entry of a subscribed GTS type after it"* — subscribed,
    /// rather than any type this backend holds.
    ///
    /// It is the mistake a backend makes by keeping **one** retention watermark
    /// rather than one row per type — the shape a deployment arrives at when
    /// every meter is swept on one timer — and also the one it makes by writing
    /// the marks table correctly and forgetting the
    /// `WHERE gts_type_id = ANY(:subscription)` on the read. It costs every
    /// consumer of every quiet meter.
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
    /// **DESIGN names this one in those words.** §3.1's `FeedPosition` row: a
    /// position's *"**encoded size** is not [plugin-internal]: the gateway
    /// carries it inside a length-bounded wire cursor whose size may not grow
    /// with a subscription's breadth. A position keyed per tenant does not meet
    /// that bound, so a plugin needs a key it can compare across a whole
    /// subscription."* §3.10's deployment-guide item 3 repeats it as the thing
    /// a plugin author has to show is not the case.
    ///
    /// It is the mistake a backend makes by having a per-tenant sequence and
    /// no order above it: it cannot issue one scalar that resumes a whole
    /// subscription, so it issues a vector, and the position grows with the
    /// customer list. The `TimescaleDB` plugin uses the `xid8` of the inserting
    /// transaction for exactly this reason — one instance-wide order, no
    /// component per tenant.
    ///
    /// **It keys on the tenants the subscription's ledger holds, not on the
    /// tenants the caller's grant names**, which is the only self-consistent
    /// version of the mistake: a position must denote the same ledger prefix
    /// under every grant (§3.1), so one naming only the grant's tenants could
    /// not be resumed by a caller whose grant had since widened. Keying on the
    /// grant would be a second defect.
    AFeedPositionIsKeyedPerTenant,
    /// Keys its feed position **per subscribed GTS type**: one component for
    /// every type named in the subscription, so its encoded size grows as a
    /// consumer adds a meter.
    ///
    /// DESIGN states the rule it breaks without qualification: *"whose size may
    /// not grow with a subscription's breadth"* (§3.1), *"which may not grow
    /// with the breadth of the subscription it positions"* (§3.3), *"however
    /// wide a subscription grows"* (§3.10). A
    /// [`FeedSubscription`](crate::feed::FeedSubscription) **is** a set of GTS
    /// types, so a position carrying one component per type is the most literal
    /// reading of the growth those forbid.
    ///
    /// It is the mistake a backend makes by keeping one feed-order sequence
    /// per meter — a partition per type is the natural physical layout, and a
    /// consumer subscribing to several then needs a cursor naming each. It is
    /// [`Self::AFeedPositionIsKeyedPerTenant`]'s mistake on the other axis, and
    /// a separate subject because the row's own contrast cannot see it: each of
    /// the subscriptions that contrast names one GTS type.
    AFeedPositionIsKeyedPerSubscribedType,
    /// Reports the `Observations` branch of `QuantitySummary` for an
    /// accruing fold — the branch `Sum` never takes — rather than the
    /// `Accrued` branch `QuantitySummary::accrues` requires of it.
    ///
    /// The mistake a backend makes by keying the branch on something other
    /// than the declared fold: a column that is `NULL` unless a count was
    /// ever computed, say, or a code path that always reports an
    /// observation and never an accrual. DESIGN §3.1 fixes the branch as a
    /// function of the fold alone — *"a summing meter reports an accrued
    /// sum, and every other meter reports how many observations the range
    /// selected together with the latest of them"* — and this subject
    /// answers the second shape where the first is owed.
    ///
    /// `reconciliation-figures`'s `accrues` case names the branch this defect
    /// gets wrong. Its `empty-range` case also dispatches a `SUM` meter and so
    /// also finds its `Accrued(0)` answer substituted; every other case
    /// dispatches a non-accruing fold and is untouched, because this defect
    /// only ever substitutes an `Accrued` answer, never the reverse.
    ReconciliationSummaryIgnoresWhetherTheFoldAccrues,
    /// Resumes a raw-page continuation by walking a fixed count of rows
    /// rather than by seeking past the boundary key `keyset` carries.
    ///
    /// **Skip, not seek.** DESIGN §3.3's plugin obligations forbid an
    /// offset/limit scan on either paginated path, and `list_usage_records`'
    /// own doc has the gateway mint `next` from exactly the structured
    /// [`crate::keyset::Keyset`] a plugin returns — a resumption built to
    /// compare against it rather than to count past it. This subject reads
    /// only `keyset.values().len()` — the order's arity, not a boundary
    /// value — and skips that many rows from the freshly selected and
    /// sorted sequence. It is the mistake a backend makes by representing its
    /// own continuation as an `OFFSET` and treating a structured keyset as a
    /// token whose only legible feature is how long it is.
    ///
    /// **Over a dense, gap-free selection a skip can agree with a seek for one
    /// continuation**, where the page limit happens to equal the fixed count.
    /// Two independent things break the agreement, one check each:
    ///
    /// * **A second continuation** — the fixed count never grows with how many
    ///   pages have gone by, so it reproduces the first continuation's view
    ///   instead of advancing. `raw-page-keyset-walk` selects five entries
    ///   across three pages at a limit equal to the order's arity.
    /// * **A page limit unequal to the fixed count**, which breaks the very
    ///   first continuation. `raw-page-caller-order` dispatches a three-key
    ///   order at a page limit of two.
    ///
    /// `raw-page-keyset-walk` also persists a hole in the selected range
    /// (`hole_fixtures`), but **the hole is not what catches this subject**:
    /// the skip runs over the already-selected, already-sorted sequence, so a
    /// hole cut in the *stored* sequence is invisible to it.
    ///
    /// A wide row, like [`Defect::DedupIgnoresTheEntryType`]'s: the mistake is
    /// one `list_usage_records` makes about its own continuation, so every
    /// check that pages past one can land on it.
    SkipsRowsRatherThanSeeksTheBoundaryKey,
}

/// The subject one defect names, ready to be handed to
/// [`run_all`](super::run_all).
///
/// Erased behind a `Box<dyn …>` because the two shapes are different types and
/// the caller has no business knowing which one a defect took: every mutant is
/// a backend the suite may simply be pointed at.
pub(super) fn mutant(defect: Defect) -> Box<dyn UsageCollectorPluginV1> {
    if carries_its_own_ledger(defect) {
        return Box::new(MutantLedger::new(defect));
    }
    Box::new(WrappedReference::new(defect))
}

/// Which of the two shapes one defect takes: a ledger of its own, or a real
/// reference backend with one method intercepted.
///
/// The routing is an exhaustive `match` rather than a default: a defect added
/// and forgotten here is a compile error, where a wildcard would silently give
/// it whichever shape the fall-through named, and a subject of the wrong shape
/// does nothing. This module's header says which defects take which shape.
///
/// It is a function rather than an arm inside [`mutant`] because two callers
/// need the answer: [`mutant`] erases the subject behind
/// `Box<dyn UsageCollectorPluginV1>`, and a driven run needs the concrete type
/// so it can lend the same value as a [`ContractRetention`] too.
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
        | Defect::AFeedPositionIsKeyedPerSubscribedType
        | Defect::ReconciliationSummaryIgnoresWhetherTheFoldAccrues => false,
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
        | Defect::FeedOrdersByTheAcceptanceInstant
        | Defect::SkipsRowsRatherThanSeeksTheBoundaryKey => true,
    }
}

// The wrapping shape

/// How many bytes one component of a keyed position carries: **sixteen**,
/// a tenant `Uuid`'s width.
///
/// One width for both keyed defects, so the arithmetic that strips them off
/// again is one rule rather than one per defect. The value is never read back —
/// `feed-position-bounded` compares how big a position is, never what it says.
const MUTANT_POSITION_COMPONENT_BYTES: usize = 16;

/// One position component standing for a subscribed GTS type.
///
/// The type id's first [`MUTANT_POSITION_COMPONENT_BYTES`] bytes, zero padded.
/// Two types sharing that prefix would share a component and the subject would
/// be none the worse for it: the components are counted, never compared.
fn type_component(meter: &MeterRef) -> [u8; MUTANT_POSITION_COMPONENT_BYTES] {
    let mut component = [0_u8; MUTANT_POSITION_COMPONENT_BYTES];
    for (slot, byte) in component.iter_mut().zip(meter.id.as_str().as_bytes()) {
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
/// One qualification, for every wrapped defect:
/// [`Self::create_usage_records`] is not pure delegation. The inner backend
/// still decides the batch, but the per-entry alignment around it — which
/// entries reach it, and where a refusal of this wrapper's own lands in the
/// answer — is code written here.
pub(super) struct WrappedReference {
    /// The conforming backend everything is delegated to.
    inner: InMemoryReferencePlugin,
    /// Which rule this subject breaks.
    defect: Defect,
    /// The period-blind dedup index [`Defect::DedupIgnoresThePeriod`] keys
    /// on: `(tenant_id, gts_type_id, idempotency_key, entry_type)` to the
    /// entry that claimed it. Unused by every other wrapped defect.
    ///
    /// A claim is recorded at admission rather than after the inner backend
    /// stores it, modelling a unique index written inside the same
    /// transaction, so it leaves a claim behind for an entry the inner backend
    /// then refuses. The entries the suite submits that way (in
    /// `at-most-one-invalidation`) are unobservable here: each carries the
    /// accepted withdrawal's derived `id`, so the claim it overwrites names the
    /// same identity.
    period_blind_keys: Mutex<BTreeMap<PeriodBlindKey, StoredUsageRecord>>,
    /// The entry-type-blind dedup index [`Defect::DedupIgnoresTheEntryType`]
    /// keys on: `(tenant_id, gts_type_id, idempotency_key, window_start,
    /// window_end)` to the entry that claimed it. Unused by every other
    /// wrapped defect.
    ///
    /// A claim is recorded at admission, as above, so it could leave a claim
    /// behind for an entry the inner backend then refuses. That is unreachable
    /// by an invariant of this index: every entry stored under a given
    /// five-tuple carries the one `id` that tuple's claim names, and the inner
    /// backend refuses only a divergent retry of a stored `id`.
    entry_type_blind_keys: Mutex<BTreeMap<EntryTypeBlindKey, StoredUsageRecord>>,
    /// The rows [`Defect::ConflictReadBackIgnoresTheEntryType`] reads a
    /// colliding entry back from: the same five components, to **every**
    /// entry accepted under them, in arrival order. Unused by every other
    /// wrapped defect.
    ///
    /// A `Vec` rather than one entry, because two rows under one five-tuple is
    /// the whole situation the defect is about. It takes the last, which is
    /// structural: an invalidation names a target that must already be stored,
    /// so it is always the later arrival.
    ///
    /// Written after the inner backend accepts rather than before — the
    /// opposite of the indexes above, and what keeps this subject wrong in one
    /// way only. It decides nothing about admission, so the mirror cannot drift
    /// and no entry the inner refused is ever read back from it.
    five_component_rows: Mutex<BTreeMap<EntryTypeBlindKey, Vec<StoredUsageRecord>>>,
    /// The GTS types retention has been driven over through this wrapper,
    /// which is the mark [`Defect::RefusesTheOldestStartAfterASweep`] reads.
    /// Unused by every other wrapped defect.
    ///
    /// Keyed by the registry reference, as the inner backend's own
    /// `retention_marks` is. A type is recorded whatever the drop removed,
    /// because a wrapper cannot see the inner backend's marks and has only the
    /// call to go on; the defect's own doc says what that costs.
    swept_types: Mutex<BTreeSet<Uuid>>,
    /// The tenants each GTS type has been written under, which is what
    /// [`Defect::AFeedPositionIsKeyedPerTenant`] issues one position
    /// component per. Unused by every other wrapped defect.
    ///
    /// **Recorded per type rather than in one set**, because that is the shape
    /// the defect is about: a per-tenant key names the tenants the
    /// subscription's types hold entries under. One set across the whole
    /// backend would make every subscription's position the same size, which
    /// would pass the check this exists to fail.
    ///
    /// A tenant is recorded at admission, like the dedup indexes above, so one
    /// whose only entry the inner backend then refused is still counted. That
    /// over-approximates the key set in the direction of a longer position, and
    /// nothing reads it but the encoder.
    tenants_written: Mutex<BTreeMap<Uuid, BTreeSet<Uuid>>>,
}

/// The inputs a period-blind dedup identity keys on:
/// `(tenant_id, gts_type_id, idempotency_key, entry_type)`, the entry type as
/// the lowercase wire literal the derivation itself reads
/// ([`crate::models::EntryType::as_str`]).
///
/// Named for what it leaves out: the covered-period bounds, which is the whole
/// of [`Defect::DedupIgnoresThePeriod`].
type PeriodBlindKey = (Uuid, Uuid, String, &'static str);

/// The inputs an entry-type-blind dedup identity keys on:
/// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`.
///
/// Named for what it leaves out: `entry_type`, the one input a record and its
/// invalidation disagree on. Two defects key on it, each striking that input
/// out of a different one of DESIGN §3.3's identity sites —
/// [`Defect::DedupIgnoresTheEntryType`] out of the unique constraint, and
/// [`Defect::ConflictReadBackIgnoresTheEntryType`] out of the read-back.
type EntryTypeBlindKey = (
    Uuid,
    Uuid,
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
    /// The subscription is scanned rather than the whole set compared, because
    /// the conforming refusal reads its marks *"per subscribed GTS type"* and a
    /// subject wrong about which types it consults would be wrong twice.
    fn a_subscribed_type_has_been_swept(
        &self,
        subscription: &[MeterRef],
    ) -> Result<bool, UsageCollectorPluginError> {
        let swept = self.swept_types.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's retention mark lock is poisoned")
        })?;
        Ok(subscription.iter().any(|meter| swept.contains(&meter.uuid)))
    }

    /// The position the feed has reached for `subscription` under `scope`,
    /// read out of the inner backend.
    ///
    /// One unbounded page: the inner backend advances its cursor past every
    /// entry it scans whether or not the page carries it, so a page that
    /// scanned the whole ledger hands back the head.
    /// [`Defect::AFeedBootstrapReadStartsAtTheHead`] substitutes it for
    /// `FeedStart::Oldest`.
    async fn the_head(
        &self,
        subscription: &[MeterRef],
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
        subscription: &[MeterRef],
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
    /// behaviour: nothing about what is stored changes, and only the key set
    /// its position encoder issues one component per is built.
    ///
    /// Synchronous, and it holds the lock only for its own body: the caller
    /// awaits the inner backend afterwards, never while holding it.
    fn remember_the_tenant(
        &self,
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        let mut written = self.tenants_written.lock().map_err(|_| {
            UsageCollectorPluginError::internal("the mutant's tenant index lock is poisoned")
        })?;
        written
            .entry(record.gts_type_uuid)
            .or_default()
            .insert(record.tenant_id);
        drop(written);
        Ok(record)
    }

    /// Whether this subject re-encodes the positions the inner backend
    /// issues.
    ///
    /// True for the defects that key a position on something growing with a
    /// subscription, false for every other wrapped defect — which hands the
    /// inner backend's own encoding straight back, byte for byte.
    fn keys_its_position(&self) -> bool {
        self.defect == Defect::AFeedPositionIsKeyedPerTenant
            || self.defect == Defect::AFeedPositionIsKeyedPerSubscribedType
    }

    /// The components this subject's position carries beside the inner
    /// backend's own encoding.
    ///
    /// One per tenant the subscribed types hold entries under, or one per type
    /// named — which of those is the defect. The component *values* are never
    /// read back, since the rule is about a position's size; they are real
    /// values all the same, because a position carrying filler would be a
    /// caricature of a plugin that has per-tenant state to name.
    fn position_components(
        &self,
        subscription: &[MeterRef],
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
        for meter in subscription {
            if let Some(under_this_type) = written.get(&meter.uuid) {
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
        subscription: &[MeterRef],
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
    /// The mirror of [`Self::the_issued_position`], and why those defects can
    /// be wrappers: the inner backend is handed back exactly the position it
    /// issued, so everything it does with a resumed position is the exemplar's.
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
    fn on_admission(
        &self,
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        match self.defect {
            // A `double precision` column: the value is whatever survives
            // the trip through the binary float.
            Defect::QuantityThroughFloat => Ok(StoredUsageRecord {
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
            Defect::StampsItsOwnAcceptedAt => Ok(StoredUsageRecord {
                accepted_at: MUTANT_INSERT_INSTANT,
                ..record
            }),
            // A `DEFAULT 'live'` column the insert never names: whichever
            // route the gateway stamped, the row records the common one.
            Defect::DefaultsOriginToLive => Ok(StoredUsageRecord {
                origin: RecordOrigin::Live,
                ..record
            }),
            Defect::DedupIgnoresThePeriod => self.claim_period_blind_key(record),
            Defect::DedupIgnoresTheEntryType => self.claim_entry_type_blind_key(record),
            Defect::AFeedPositionIsKeyedPerTenant => self.remember_the_tenant(record),
            // Enumerated rather than caught by a wildcard: a new defect routed
            // here and forgotten would otherwise pass its entries through
            // untouched and report no violation at all, which is a matrix row
            // whose subject does nothing. Every variant below is applied
            // somewhere else (the read path, around the inner call, the fold,
            // the feed path) or never reaches this type at all.
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
            | Defect::FeedOrdersByTheAcceptanceInstant
            | Defect::ReconciliationSummaryIgnoresWhetherTheFoldAccrues
            | Defect::SkipsRowsRatherThanSeeksTheBoundaryKey => Ok(record),
        }
    }

    /// Admits an entry only if no other entry already holds its
    /// `(tenant, type, idempotency_key, entry_type)`.
    ///
    /// A unique index over the derived identity with the covered period struck
    /// out, and that omission is the whole of the defect: one key over two
    /// periods collides here and is one entry rather than two.
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
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        let key = (
            record.tenant_id,
            record.gts_type_uuid,
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
    /// A unique index over the derived identity with `entry_type` struck out,
    /// and that omission is the whole of the defect. An invalidation repeats
    /// every caller-supplied field of its target but the reason code, so this
    /// index sees the record's own five components arrive a second time and
    /// answers the collision DESIGN names: *"A plugin that deduplicates on the
    /// other five components alone treats every invalidation as a collision
    /// with its target."*
    ///
    /// The covered period is in the index for the same reason the entry type
    /// is in [`Self::claim_period_blind_key`]'s: an index blind to the bounds
    /// as well would be wrong in a second way.
    ///
    /// A resubmission of an entry that already claimed the key carries the
    /// same derived `id`, so an idempotent replay still reaches the inner
    /// backend and is answered by it.
    fn claim_entry_type_blind_key(
        &self,
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        let key = (
            record.tenant_id,
            record.gts_type_uuid,
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
        record: &StoredUsageRecord,
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

    /// What the post-inner defects **this method decides** do to one entry's
    /// outcome, and the place [`Defect::ConflictReadBackIgnoresTheEntryType`]
    /// keeps its mirror of the ledger up to date:
    ///
    /// * [`Defect::AbsorbsAWithdrawalWithAnotherReason`]: a conflict on a
    ///   withdrawal is answered as an absorb of the stored entry.
    /// * [`Defect::ConflictReadBackIgnoresTheEntryType`]: an outcome the
    ///   inner backend decided against a *stored* entry is decided again
    ///   here, against whichever entry a five-component read-back finds.
    ///
    /// Both leave admission alone, which is why they sit here rather than in
    /// [`Self::on_admission`]. [`Defect::ConflictNamesTheRecord`] is post-inner
    /// too but needs an SPI read, so
    /// [`Self::conflict_against_the_withdrawn_record`] applies it afterwards.
    fn after_admission(
        &self,
        record: &StoredUsageRecord,
        outcome: Result<StoredUsageRecord, UsageCollectorPluginError>,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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
    /// remembered here, so the caller is handed the exemplar's own stored row,
    /// server fields included — what a backend resolving `invalidates` in its
    /// conflict branch hands back. Nothing else moves: an acceptance is passed
    /// through, and so is a conflict raised for a record.
    ///
    /// Async, which is why it is applied **after**
    /// [`Self::after_admission`] rather than inside it, on both create paths —
    /// so a conflict this subject rewrites is one the exemplar decided.
    ///
    /// A read-back that fails leaves the inner backend's own conflict in place.
    /// It is unreachable, since an invalidation is only projectable against a
    /// target the store already holds; a fall-through rather than an `expect`
    /// keeps a driven subject reporting the exemplar's answer if that ever
    /// stops holding.
    async fn conflict_against_the_withdrawn_record(
        &self,
        record: &StoredUsageRecord,
        outcome: Result<StoredUsageRecord, UsageCollectorPluginError>,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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
    /// when the insert was a no-op — the only case a real backend's read-back
    /// runs in, since `ON CONFLICT … DO NOTHING` returns no row to compare
    /// against. A fresh entry is passed through untouched and remembered.
    ///
    /// The read-back takes the last row under the five components, and the
    /// comparison against it is the ordinary one
    /// ([`StoredUsageRecord::caller_supplied_eq`]). Only *which row* is wrong
    /// here, which is the whole of the defect.
    fn read_the_conflict_back_on_five_components(
        &self,
        record: &StoredUsageRecord,
        outcome: Result<StoredUsageRecord, UsageCollectorPluginError>,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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
        // key carries the submitted `id`. A fall-through rather than an
        // `expect`, so a driven subject reports the inner backend's own answer
        // if that ever stops holding rather than aborting the run.
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
    /// returns a withdrawn pair as persisted, so they are the keys a `GROUP BY`
    /// sees when the withdrawal exclusion sits in a `FILTER (WHERE …)` on the
    /// aggregate rather than in the `WHERE`. A key the grouping drops for a
    /// missing value is dropped here too, because [`bucket_key`] is the same
    /// function the mirror groups with: this subject is wrong about which
    /// groups exist, not about how one is keyed. Each added bucket carries
    /// [`fold_value`]'s own answer over no rows, so the subject is wrong in one
    /// way only.
    ///
    /// The page is bounded by the caller's own `query.limit`, so a key carried
    /// only by rows past that limit is not added. No grouped dispatch the suite
    /// makes comes near its limit; a check grouping over a wider range would
    /// have to widen the limit with it.
    #[expect(
        clippy::too_many_arguments,
        reason = "the SPI's own aggregate signature, plus the delegated answer this rewrites; \
                  bundling them into a struct would name the SPI's parameters twice"
    )]
    async fn bucket_every_selected_key(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
        answered: AggregationResult,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let page = self
            .inner
            .list_usage_records(meter, time_range, query, metadata_filter, None)
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

/// The caller-supplied identity components of `record`, `entry_type` struck
/// out.
fn five_component_key(record: &StoredUsageRecord) -> EntryTypeBlindKey {
    (
        record.tenant_id,
        record.gts_type_uuid,
        record.idempotency_key.as_str().to_owned(),
        record.window_start,
        record.window_end,
    )
}

#[async_trait]
impl UsageCollectorPluginV1 for WrappedReference {
    /// The batch, with the defect applied per entry and the survivors handed
    /// to the inner backend in **one** call.
    ///
    /// Passing the survivors as a batch rather than admitting them one at a
    /// time keeps the resolution of a later same-identity entry against an
    /// earlier one in the same call the inner backend's to decide. Admitting
    /// them singly would mean the other wrapped subjects passed
    /// `at-most-one-invalidation`'s one-batch half for a reason of the
    /// wrapper's own rather than the exemplar's.
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
        let mut prepared: Vec<Result<(MeterRef, StoredUsageRecord), UsageCollectorPluginError>> =
            Vec::with_capacity(records.len());
        for (meter, record) in records {
            let entry = match self.on_admission(record) {
                Ok(record) => match self.refused_as_a_second_withdrawal(&record).await {
                    Some(refusal) => Err(refusal),
                    None => Ok((meter, record)),
                },
                Err(err) => Err(err),
            };
            prepared.push(entry);
        }
        let survivors: Vec<(MeterRef, StoredUsageRecord)> = prepared
            .iter()
            .filter_map(|entry| entry.as_ref().ok().cloned())
            .collect();
        // A batch every entry of which this wrapper refused must not reach
        // the inner backend: the reference answers an empty batch with
        // `Internal`, and this call would then fail outright rather than
        // reporting the per-entry refusals it already has.
        //
        // A live path rather than a precaution, which is why the guard is
        // repeated here rather than left to the inner backend. Of the three
        // batches the suite sends, exactly one can empty this list:
        // `at-most-one-invalidation`'s two withdrawals of an already-stored
        // target, under `DedupIgnoresTheEntryType`. `dedup-floor`'s two batches
        // are pairs of records sharing all six identity inputs, so each pair
        // derives one `id` and reaches the inner backend whole — which is the
        // point, since the in-call resolution stays the exemplar's.
        //
        // The reasoning is per subject rather than general: a later defect that
        // reaches this branch a second way has to say so here.
        let inner = if survivors.is_empty() {
            Vec::new()
        } else {
            self.inner.create_usage_records(survivors).await?
        };
        let mut inner = inner.into_iter();
        // A loop rather than a `map`, because one post-inner defect is async:
        // `conflict_against_the_withdrawn_record` reads the withdrawn record
        // back from the inner backend, and a closure cannot await. The
        // alignment is one inner outcome consumed per entry this wrapper passed
        // on, in order, with a refusal of its own kept where the entry stood.
        let mut answers = Vec::with_capacity(prepared.len());
        for entry in prepared {
            match entry {
                Ok((_, record)) => {
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
    /// [`Defect::AnswersNotConvergedForAnAcknowledgedEntry`] leaves the query
    /// alone and rewrites one answer: a converged-only lookup that found the
    /// entry reports it undecided instead. **Only that answer** — a refusal is
    /// passed through, so an identifier that was never stored and a row the
    /// scope withheld both still read as absent.
    ///
    /// The two are exclusive branches rather than one composed path: each
    /// subject carries exactly one defect.
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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
    /// **A second dispatch is what tells an empty `SUM` bucket from one whose
    /// rows sum to zero**, and the two have to be told apart: a subject
    /// answering absent to both would be wrong about a rule no check here asks
    /// it about. The exemplar's own `COUNT` over the same arguments is the
    /// signal — same selection, exclusion and grouping, so its buckets carry
    /// the same keys and a zero count is exactly an empty selection.
    /// Re-implementing the fold here would make this a [`MutantLedger`] rather
    /// than a wrapper. The other two folds need no probe: an absent `MAX` *is*
    /// an empty selection, and a missing bucket is read off the ledger page.
    ///
    /// The `SUM` rewrite is keyed on the bucket key rather than on position, so
    /// it does not rest on two calls returning their buckets in one order. No
    /// grouped bucket the exemplar emits is ever empty — a group is keyed from
    /// a surviving row — so in practice it only touches the no-grouping case.
    async fn query_aggregated_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let result = self
            .inner
            .query_aggregated_usage_records(
                meter,
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
                        meter,
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
                    meter,
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
        meter: &MeterRef,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        self.inner
            .list_usage_records(meter, time_range, query, metadata_filter, keyset)
            .await
    }

    /// Delegated, with `FeedStart::Oldest` reinterpreted by the bootstrap
    /// defects and the page's continuation re-encoded by the two that are
    /// about what a position costs to carry.
    ///
    /// The bootstrap defects land **above** the page loop rather than inside
    /// it, which is why they can be interceptions where the feed defects in
    /// [`MutantLedger`] cannot: two substitute a different `start` and one
    /// declines to serve the call. Everything after that decision is the
    /// exemplar's own loop, cursor and page.
    ///
    /// The position defects land **below** it, on the position the loop
    /// produced. [`Self::the_inner_position`] strips a subject's own components
    /// off whatever it is handed and [`Self::the_issued_position`] puts them
    /// back on the way out, so the inner backend is resumed at exactly the
    /// position it issued and what differs is the size of the token a caller
    /// carries. Both are no-ops for every other wrapped defect.
    ///
    /// `FeedStart::After` is delegated untouched by the bootstrap defects.
    /// It is matched with a wildcard arm because [`FeedStart`] is
    /// `#[non_exhaustive]`, and a start mode added later is one neither of
    /// them has an opinion about.
    async fn read_feed_page(
        &self,
        subscription: &[MeterRef],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
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

    /// Delegated, with [`Defect::ReconciliationSummaryIgnoresWhetherTheFoldAccrues`]
    /// substituting an empty `Observations` summary wherever the exemplar
    /// answered `Accrued` — the wrong branch for the declared fold. No
    /// other defect routed to this wrapper touches reconciliation.
    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        let answer = self
            .inner
            .get_reconciliation_metadata(tenant_id, meter, time_range, fold, scope)
            .await?;
        if self.defect != Defect::ReconciliationSummaryIgnoresWhetherTheFoldAccrues {
            return Ok(answer);
        }
        if !matches!(answer.quantity_summary, QuantitySummary::Accrued(_)) {
            return Ok(answer);
        }
        Ok(ReconciliationMetadata {
            quantity_summary: QuantitySummary::Observations(None),
            ..answer
        })
    }
}

/// The retention drive, delegated to the exemplar and recorded.
///
/// Implemented on the shape rather than on the one defect that reads the
/// record, so any defect routed here can be handed to
/// [`run_all_with_retention`](super::run_all_with_retention) without growing a
/// second shape. The sweep itself is the exemplar's, so a subject under a drive
/// is still the reference backend plus exactly its own named mistake.
#[async_trait]
impl ContractRetention for WrappedReference {
    /// Records the type and hands the drive to the inner backend.
    ///
    /// The record is taken **before** the delegation, like the two dedup
    /// indexes' claims: it models a write inside the same transaction as the
    /// sweep, and a mark raised for a sweep that then failed is a mark a real
    /// backend would also be left holding.
    ///
    /// # Errors
    ///
    /// Whatever the inner backend answers, plus a poisoned-lock report of
    /// this wrapper's own.
    async fn drop_before(
        &self,
        meter: &MeterRef,
        floor: time::OffsetDateTime,
    ) -> Result<(), String> {
        self.swept_types
            .lock()
            .map_err(|_| "the mutant's retention mark lock is poisoned".to_owned())?
            .insert(meter.uuid);
        self.inner.drop_before(meter, floor).await
    }
}

/// The subject one defect names, as a concrete type a driven run can hold.
///
/// [`mutant`] erases its subject behind `Box<dyn UsageCollectorPluginV1>`,
/// which is right for the discrimination matrix and useless for
/// [`run_all_with_retention`](super::run_all_with_retention): that entry point
/// wants the same backend as a [`ContractRetention`] too, and a value erased
/// behind one of the two traits cannot be recovered as the other. This returns
/// the wrapper itself, so a caller can lend it as both.
/// [`drivable_ledger_mutant`] is the same for the other shape.
pub(super) fn drivable_mutant(defect: Defect) -> WrappedReference {
    WrappedReference::new(defect)
}

/// One round trip of a quantity through a binary float.
///
/// Most of the published range's corners move, which is what a
/// `double precision` column costs and what `quantity-round-trip` exists to
/// find: both magnitude corners lose their low digits
/// (`9999999999999999999999999999` comes back
/// `9999999999999999583119736832`), and `42.500` comes back `42.5`, its
/// scale normalised away.
///
/// **The `1e-28` corners survive, and a real float column would not lose them
/// either**, because `Decimal`'s 28-digit scale cap truncates the float's
/// excess digits back onto the original value. This subject is as wrong as the
/// column it models rather than kinder than it.
///
/// `unwrap_or(value)` is a floor for a value the carrier could not take back at
/// all, and it is **unreached**: `from_f64` answers `Some` for every quantity
/// this suite submits. It is here so a future corner cannot turn this function
/// into a panic.
fn through_f64(value: Decimal) -> Decimal {
    value.to_f64().and_then(Decimal::from_f64).unwrap_or(value)
}

/// The instant [`Defect::StampsItsOwnAcceptedAt`] writes in place of the one
/// it was handed.
///
/// **A fixed instant of this subject's own, never `OffsetDateTime::now_utc()`
/// — which is what the `DEFAULT now()` column it models would really write.**
/// A subject whose answer moves between runs makes a failure unreproducible,
/// and nothing here needs to be "now" in order to be wrong: the check compares
/// what a read answered against what it submitted, never against this value.
///
/// Later than every acceptance instant the suite submits, which is the
/// direction a real insert time lies in. `CONTRACT_ACCEPTED_AT` is the UNIX
/// epoch plus `20_454` days and `server-field-round-trip` offsets its own four
/// from that by hours; this is `21_000` days, clear of all of them, and that
/// distinctness is what makes the substitution observable.
///
/// **This subject reaches every assertion `server-field-round-trip` makes**,
/// which bounds what the matrix row proves: admission is upstream of every read
/// path and of the absorb, so no single assertion is *necessary* for the row. It
/// establishes that the check as a whole notices a backend stamping its own
/// instant, not that any one path's assertion is load-bearing.
/// [`Defect::DefaultsOriginToLive`] is the same shape and carries the same
/// caveat. Which of the check's fields has a subject at all is stated in this
/// module's header.
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

// The own-ledger shape

/// How many admission sequences [`MutantLedger::feed_key`] reserves under
/// each whole second of a gateway-stamped acceptance instant.
///
/// Used by one subject alone, [`Defect::FeedOrdersByTheAcceptanceInstant`],
/// to keep a composite key unique. A power of two rather than a round decimal
/// because nothing reads it back: it is headroom, not a unit.
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
    record: StoredUsageRecord,
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
    /// Per meter, the highest sequence retention has removed, keyed by the
    /// registry reference — the value this ledger keys everything on, and
    /// `Ord` where a [`crate::MeterTypeId`] is not.
    ///
    /// Raised by [`Self::drop_before`], so it stays empty under
    /// [`super::run_all`], which drives no sweep.
    retention_marks: BTreeMap<Uuid, u64>,
    /// The entries a sweep removed, with the sequences they were admitted
    /// under.
    ///
    /// **Read by one defect alone**,
    /// [`Defect::TheRetentionRefusalReadsTheCallersGrant`], which decides the
    /// refusal from what the caller's grant would have carried and therefore
    /// needs the rows rather than a high-water mark. The reference backend
    /// needs no such list: its refusal reads [`Self::retention_marks`] and
    /// nothing else, which is the whole of what DESIGN asks for.
    ///
    /// A mirror of what a sweep took rather than a second ledger: no read path
    /// consults it and nothing is ever removed from it.
    removed: Vec<Entry>,
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

    /// The admitted records in admission order.
    ///
    /// Every path but the feed reads the ledger through this, as in the
    /// reference: only the feed has a position to seek to, so only it reads
    /// the sequences.
    fn records(&self) -> impl Iterator<Item = &StoredUsageRecord> {
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
    fn replace(&mut self, record: StoredUsageRecord) {
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
    fn retention_has_passed(&self, subscription: &[MeterRef], position: u64) -> bool {
        subscription.iter().any(|meter| {
            self.retention_marks
                .get(&meter.uuid)
                .is_some_and(|mark| *mark > position)
        })
    }

    /// Removes every entry of `gts_type_uuid` whose covered period ends
    /// before `floor`, raising that meter's mark to the highest sequence removed, as
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
    fn drop_before(&mut self, gts_type_uuid: Uuid, floor: time::OffsetDateTime) {
        let mut kept = Vec::with_capacity(self.entries.len());
        for entry in std::mem::take(&mut self.entries) {
            if entry.record.gts_type_uuid != gts_type_uuid || entry.record.window_end >= floor {
                kept.push(entry);
                continue;
            }
            let mark = self.retention_marks.entry(gts_type_uuid).or_default();
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
    fn a_subscribed_type_has_been_swept(&self, subscription: &[MeterRef]) -> bool {
        subscription
            .iter()
            .any(|meter| self.retention_marks.contains_key(&meter.uuid))
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
        subscription: &[MeterRef],
        scope: &ast::Expr,
        position: u64,
    ) -> bool {
        self.removed.iter().any(|entry| {
            entry.sequence > position
                && subscription
                    .iter()
                    .any(|meter| meter.uuid == entry.record.gts_type_uuid)
                && expr_admits(&entry.record, scope)
        })
    }
}

/// A ledger of this module's own, for the defects a wrapper cannot reach.
///
/// A wrapper cannot change a predicate the inner backend owns (which column a
/// range meets, which rows a fold walks), nor make
/// [`InMemoryReferencePlugin`] hold or move a row its own admission refuses.
/// This type covers those, plus the `LATEST` orders, the page loop and the
/// refusal guarding it — see this module's header.
///
/// Everything else mirrors [`InMemoryReferencePlugin`]: the same admission
/// decision, the same `from <= window_end < to` selection, the same
/// withdrawal exclusion, the same `(window_end, id)` page order, the same
/// sequence-stamped feed with its seek, cursor and retention refusal, the same
/// grouping and fold values, and the same reconciliation figures.
///
/// **One stated way it is smaller than the reference, reaching no check.** Its
/// filter evaluator translates exactly the shapes the suite dispatches —
/// `And`, `Or`, and `<identifier> eq <literal>` over `tenant_id` and
/// `resource_type` — and nothing else, so the two evaluators agree on every
/// expression the suite produces.
///
/// # How far the mirror is pinned
///
/// Every check [`run_all`](super::run_all) runs is *passed* by at least one
/// subject built on this type, so a mirrored behaviour that drifted from the
/// reference would make some row report a violation its expected set does not
/// name. Drift is a test failure rather than something a reader has to notice.
///
/// **The pin reaches exactly what `run_all` dispatches**, which is less than
/// the whole mirror. Pinned: the covered-period bound, the admission decision,
/// the withdrawal exclusion, the ungrouped `SUM` and `COUNT`, which rows the
/// three scope-carrying read paths answer with, and that the feed delivers a
/// subscribed meter's entries at all.
///
/// The feed's own decisions are pinned by their subjects —
/// [`Defect::FeedCursorCountsAdmittedEntries`] for the scanned-entry cursor,
/// [`Defect::FeedOrdersByTheAcceptanceInstant`] for the walk order,
/// [`Defect::AFeedPageDropsTheEntryAtItsLimit`] for where the cursor stops
/// against the limit, and [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`]
/// for the seek comparison — plus the scope and subscription gates, pinned by
/// `feed-snapshot-and-replay`'s walk.
///
/// The seek and the retention refusal are pinned **only under**
/// [`super::run_all_with_retention`]: a dense append-ordered ledger makes a
/// `skip` indistinguishable from a seek until a retention gap separates them.
///
/// Still **unpinned** and level with the reference: the ledger page's sort
/// (its membership is pinned), the grouped folds, the three folds reading a
/// quantity, and the reconciliation figures. Each becomes pinned by the check
/// that first dispatches it; until then this is where the mirror can rot in
/// silence.
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
    /// **The one line [`Defect::SelectsOnWindowStart`] changes**, a method
    /// rather than an expression inside [`Self::selects`] because a backend
    /// that ported the pre-period point-in-time column meets every range on it:
    /// the read paths' selection and the reconciliation figures alike.
    fn range_bound(&self, entry: &StoredUsageRecord) -> time::OffsetDateTime {
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
    /// **Under this subject a position is a composite key rather than a
    /// sequence**, while [`Ledger`]'s retention marks are sequences. Comparing
    /// the two would mean nothing, and never happens: this subject is a row of
    /// `contract_tests`' undriven discrimination matrix only, and
    /// [`super::run_all`] drives no sweep. Whoever drives it first has to
    /// reconcile the two keys here before reading anything into the result.
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
    /// **This is why those two defects carry a ledger of their own rather than
    /// wrapping the reference**: they change what the backend *stores*, and a
    /// wrapper cannot make [`InMemoryReferencePlugin`] hold a second row under
    /// one `id` nor write one row over another — its own admission refuses
    /// both. Keeping the extra rows in a side ledger would mean
    /// re-implementing every read path to merge them back in, which is this
    /// type with extra steps.
    fn admit_here(
        &self,
        ledger: &mut Ledger,
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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
    /// [`WrappedReference::on_admission`]'s: here the fall-through is the
    /// *conforming* rule, which is what every defect this method is not about
    /// already wants.
    fn refuses_the_cursor(
        &self,
        ledger: &Ledger,
        subscription: &[MeterRef],
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
        entry: &StoredUsageRecord,
        gts_type_uuid: Uuid,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> bool {
        let filter_admits = match query.filter() {
            Some(filter) => expr_admits(entry, filter),
            None => true,
        };
        filter_admits
            && entry.gts_type_uuid == gts_type_uuid
            && time_range.contains_window_end(self.range_bound(entry))
            && metadata_admits(entry, metadata_filter)
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for MutantLedger {
    /// The batch, decided under one lock in input order.
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
        let records: Vec<StoredUsageRecord> = records
            .into_iter()
            .map(|(MeterRef { uuid: _, id: _ }, record)| record)
            .collect();
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
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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
    /// alone, which is why the row filter above is blind to all three.
    ///
    /// Everything after the row filter — the grouping, the bucket cap, the
    /// split an empty selection makes by fold — is [`fold_rows`]'s, which
    /// mirrors the reference's function of the same name.
    async fn query_aggregated_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let withdrawn = withdrawn_targets(&ledger);
        let counts_invalidations = self.defect == Defect::FoldsTheInvalidation;
        let mut rows: Vec<&StoredUsageRecord> = Vec::new();
        for entry in ledger.records() {
            if self.selects(entry, meter.uuid, time_range, query, metadata_filter)
                && (counts_invalidations || entry.invalidation.is_none())
                && !withdrawn.contains(&entry.id)
            {
                rows.push(entry);
            }
        }
        fold_rows(fold, &rows, group_by, LatestOrder::of(self.defect))
    }

    /// A page from the selected set, mirroring the reference's
    /// order-honouring sort, seek and keyset — see `reference.rs`'s
    /// `list_usage_records` for the rule this follows, with the selection
    /// alone routed through [`Self::selects`] so [`Defect::SelectsOnWindowStart`]
    /// still reaches this path.
    ///
    /// [`Defect::SkipsRowsRatherThanSeeksTheBoundaryKey`] is the one thing
    /// this method does not mirror: its own doc says what it substitutes and
    /// why.
    async fn list_usage_records(
        &self,
        meter: &MeterRef,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        let ledger = self.ledger()?;
        let mut items: Vec<StoredUsageRecord> = ledger
            .records()
            .filter(|entry| self.selects(entry, meter.uuid, time_range, query, metadata_filter))
            .cloned()
            .collect();

        // Three of the four steps are the reference's own functions, not a
        // second copy of them: `order_is_ascending`, `sort_by_dispatched_order`
        // and `page_with_continuation` are `pub(super)` in `reference.rs` for
        // exactly this, which keeps the typed comparison single-sourced. See
        // [`crate::contract::reference::OrderFieldValue`]'s doc for why a
        // rendering comparison is unsound (an RFC 3339 instant with a zero
        // sub-second part renders *after* one with a non-zero part, lexically,
        // which is not chronological order).
        //
        // The fourth step, the seek, is the one this mirror substitutes, under
        // `Defect::SkipsRowsRatherThanSeeksTheBoundaryKey`. It stays here
        // rather than becoming a parameter on the reference's helper, because
        // a defect switch inside the exemplar is what this module's own doc
        // says must not exist.
        let ascending = order_is_ascending(query);
        sort_by_dispatched_order(&mut items, query, ascending);

        let items: Vec<StoredUsageRecord> = match keyset {
            Some(ks) if self.defect == Defect::SkipsRowsRatherThanSeeksTheBoundaryKey => {
                // Skip, not seek: walk `keyset.values().len()`-many rows past
                // the start of the already-selected, already-sorted sequence
                // rather than comparing against the boundary key. See this
                // defect's own doc for which two checks that disagrees with a
                // seek on.
                items.into_iter().skip(ks.values().len()).collect()
            }
            Some(ks) => seek_past_boundary(items, query, ks, ascending)?,
            None => items,
        };

        page_with_continuation(items, query, ascending)
    }

    /// A page from the ledger's own append order, mirroring the reference's.
    ///
    /// Three properties are mirrored here. The resumption is a **seek**:
    /// `sequence > from` resumes at the first entry the position does not
    /// already cover, never a count of entries walked past, which is DESIGN
    /// §3.3's *"Offset/limit scans are forbidden on both paginated paths"*. The
    /// cursor advances past every entry **scanned** rather than every entry
    /// **admitted**, so a position denotes the same ledger prefix under every
    /// grant. And a cursor whose continuation retention has truncated is
    /// refused (`feed-retention-refusal`); [`Ledger::drop_before`] raises the
    /// mark it reads, and only [`super::run_all_with_retention`] drives one.
    ///
    /// **The defects routed here that touch the feed** split in two. Those
    /// inside the page loop: [`Defect::FeedOrdersByTheAcceptanceInstant`] walks
    /// the ledger in another order (through [`MutantLedger::feed_order`]),
    /// [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`] backs the resumption
    /// up by one, [`Defect::FeedCursorCountsAdmittedEntries`] moves the cursor
    /// assignment inside the admission branch so the cursor stops short,
    /// [`Defect::AFeedPageDropsTheEntryAtItsLimit`] checks the limit after the
    /// cursor has moved so it runs one entry past, and
    /// [`Defect::ABoundedReplayNeverCloses`] keeps minting a continuation past
    /// the `until`. The rest decide, *before* the loop, whether the page is
    /// served at all, and live in [`MutantLedger::refuses_the_cursor`].
    /// Everything else here is a mirror and nothing more.
    async fn read_feed_page(
        &self,
        subscription: &[MeterRef],
        scope: &ast::Expr,
        start: FeedStart<FeedPosition>,
        until: Option<FeedPosition>,
        limit: u64,
    ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
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
            // stopped at, leaving that entry in front of the cursor.
            // [`Defect::AFeedPageDropsTheEntryAtItsLimit`] stops after instead.
            let full = u64::try_from(entries.len()).unwrap_or(u64::MAX) >= limit;
            if full && self.defect != Defect::AFeedPageDropsTheEntryAtItsLimit {
                break;
            }
            // The cursor moves onto this entry's key whether or not the
            // subscription and the scope admit it. Advancing only past admitted
            // entries would make the position depend on who asked — which is
            // what [`Defect::FeedCursorCountsAdmittedEntries`] does.
            let carried = subscription
                .iter()
                .any(|meter| meter.uuid == entry.record.gts_type_uuid)
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
        meter: &MeterRef,
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
                .filter(|entry| entry.tenant_id == tenant_id && entry.gts_type_uuid == meter.uuid)
        };
        let in_range =
            || in_scope().filter(|entry| time_range.contains_window_end(self.range_bound(entry)));

        let accepted_count = in_range().count();
        let folded: Vec<&StoredUsageRecord> = in_range()
            .filter(|entry| entry.invalidation.is_none() && !withdrawn.contains(&entry.id))
            .collect();

        Ok(ReconciliationMetadata {
            accepted_count: u64::try_from(accepted_count).unwrap_or(u64::MAX),
            quantity_summary: if QuantitySummary::accrues(fold) {
                QuantitySummary::Accrued(accrued_sum(&folded)?)
            } else {
                QuantitySummary::Observations(
                    LatestOrder::of(self.defect)
                        .pick(&folded)
                        .map(|row| row.quantity)
                        .map(|latest| ObservedQuantity {
                            count: NonZeroU64::new(u64::try_from(folded.len()).unwrap_or(u64::MAX))
                                .expect(
                                    "`LatestOrder::pick` returns `Some` only when `folded` is \
                                     non-empty",
                                ),
                            latest,
                        }),
                )
            },
            max_accepted_at: in_scope().map(|entry| entry.accepted_at).max(),
            max_window_end: in_scope().map(|entry| entry.window_end).max(),
        })
    }
}

/// The retention drive, mirroring the reference's.
///
/// Implemented on the shape rather than on the defects that read a mark, so any
/// defect routed here can be handed to
/// [`run_all_with_retention`](super::run_all_with_retention). That is what puts
/// a mirror subject in front of the driven assertions at all, which is how the
/// mirror's feed **seek** is pinned — `contract_tests`'
/// `a_subject_carrying_its_own_ledger_is_still_itself_under_a_drive` spends it.
#[async_trait]
impl ContractRetention for MutantLedger {
    /// Runs this subject's retention over one GTS type, to one floor.
    ///
    /// The sweep is [`Ledger::drop_before`]'s, which is the reference's rule:
    /// every entry of the type whose covered period ends before `floor` is
    /// removed and the type's mark rises to the highest sequence removed, both
    /// under one lock acquisition.
    ///
    /// **No defect in this module is about the sweep**, and none should be: a
    /// subject whose sweep removed the wrong rows would be a harness setting up
    /// the wrong scenario rather than a backend answering an SPI call wrongly.
    ///
    /// # Errors
    ///
    /// A message naming the poisoned ledger lock, the one way this can fail.
    async fn drop_before(
        &self,
        meter: &MeterRef,
        floor: time::OffsetDateTime,
    ) -> Result<(), String> {
        let MeterRef { uuid, id: _ } = meter;
        let mut ledger = self
            .ledger()
            .map_err(|err| format!("the mutant ledger could not run retention: {err}"))?;
        ledger.drop_before(*uuid, floor);
        Ok(())
    }
}

/// The subject one defect names, as a concrete ledger-carrying type a driven
/// run can hold.
///
/// [`drivable_mutant`] is this for the wrapped shape and says why the erasure
/// [`mutant`] applies is useless to
/// [`run_all_with_retention`](super::run_all_with_retention).
///
/// Both shapes are offered because the four retention-refusal defects live
/// inside `read_feed_page`'s own refusal branch, which a wrapper cannot reach:
/// the branch is the inner backend's, and no interception can make a conforming
/// backend fail to refuse.
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
    record: &StoredUsageRecord,
) -> Result<Option<StoredUsageRecord>, UsageCollectorPluginError> {
    decide_against(ledger.records(), record)
}

/// The same decision over an arbitrary set of rows.
///
/// Split out for [`resolve_against_the_pre_call_ledger`], which decides a whole
/// batch against a snapshot rather than against the live ledger. The decision
/// itself stays in one function: a second copy is how a mirror starts
/// disagreeing with the exemplar in a place no defect names.
fn decide_against<'a>(
    rows: impl IntoIterator<Item = &'a StoredUsageRecord>,
    record: &StoredUsageRecord,
) -> Result<Option<StoredUsageRecord>, UsageCollectorPluginError> {
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
/// **The write is the ordinary one.** [`admit`] still refuses a duplicate `id`,
/// because the unique constraint is DESIGN §3.3's *first* named site and
/// [`Defect::LedgerHasNoUniqueConstraint`] strikes that one out. So the row the
/// constraint refuses is dropped without a word — an
/// `ON CONFLICT … DO NOTHING` — and the caller is told a row was accepted that
/// the store never wrote.
///
/// A same-identity pair that is **identical** is indistinguishable here from a
/// conforming absorb, necessarily: both answer `Ok` carrying an entry equal in
/// every caller-supplied field, and only the row count could tell them apart —
/// which the surviving constraint keeps at one. The divergent pair is where
/// this subject shows.
fn resolve_against_the_pre_call_ledger(
    ledger: &mut Ledger,
    records: Vec<StoredUsageRecord>,
) -> Vec<Result<StoredUsageRecord, UsageCollectorPluginError>> {
    let before: Vec<StoredUsageRecord> = ledger.records().cloned().collect();
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
    record: StoredUsageRecord,
) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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
/// The answer is the conforming one in all three cases, because it is
/// [`decide`]'s — the same function [`admit`] consults. Only the write differs,
/// by not consulting anything: the schema this models has no unique constraint
/// on the dedup identity, so the insert after the lookup cannot be refused.
///
/// [`decide`] finds a row by `id` with `Iterator::find`, so it goes on
/// answering with the **first** row written under an identity however many
/// duplicates pile up behind it. That keeps this subject wrong in one way only:
/// a caller reads the outcomes a conforming backend would give, and the ledger
/// holds rows a conforming backend would not.
fn admit_without_a_unique_constraint(
    ledger: &mut Ledger,
    record: StoredUsageRecord,
) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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
/// The answer is the conforming one in all three cases, because it is
/// [`decide`]'s — the same function [`admit`] consults — and it is taken
/// *before* the write. Only the store differs: the conflicting submission
/// replaces the row it collided with instead of being dropped, which is
/// `ON CONFLICT (the six identity columns) DO UPDATE` where the conforming
/// statement is `DO NOTHING`.
///
/// An identical re-delivery writes nothing, which keeps this subject wrong in
/// one way only: the divergent branch is the whole of the defect.
fn admit_displacing_the_survivor(
    ledger: &mut Ledger,
    record: StoredUsageRecord,
) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
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

/// Every `StoredUsageRecord.id` an accepted invalidation names.
fn withdrawn_targets(ledger: &Ledger) -> BTreeSet<Uuid> {
    ledger
        .records()
        .filter_map(|entry| entry.invalidation.as_ref().map(|inv| inv.target))
        .collect()
}

/// Whether an entry satisfies every metadata filter in the slice.
fn metadata_admits(entry: &StoredUsageRecord, filters: &[MetadataFilter]) -> bool {
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
fn expr_admits(entry: &StoredUsageRecord, expr: &ast::Expr) -> bool {
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
/// An arm no dispatch reaches would be untested code inside a subject whose
/// whole job is to be wrong in one known place.
fn field_matches(entry: &StoredUsageRecord, name: &str, value: &ast::Value) -> bool {
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
    rows: &[&StoredUsageRecord],
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
fn bucket_key(row: &StoredUsageRecord, group_by: &[AggregationDimension]) -> Option<Vec<String>> {
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
/// and answer `None`.
///
/// **`LATEST` takes whichever order [`LatestOrder`] names**, which is
/// [`LatestOrder::Declared`] — the whole of DESIGN §3.1's three keys — for
/// every subject but the three that exist to get one of those keys wrong.
/// Under the declared order `id` is compared as bytes by [`Uuid`]'s derived
/// `Ord`, so the order is total and the answer never depends on ledger
/// insertion order.
fn fold_value(
    fold: AggregationFold,
    rows: &[&StoredUsageRecord],
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
/// greatest `id` in byte order"*, and one variant here strikes out each of the
/// three. One enum rather than three branches on [`Defect`], because that makes
/// the set visibly complete: a fourth way to get this order wrong would be a
/// fourth key, and there is no fourth key.
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
            // Enumerated rather than caught by a wildcard: a fold defect added
            // later and forgotten here would silently fold under DESIGN's order
            // and report nothing, which is a matrix row whose subject does
            // nothing rather than a compile error.
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
            | Defect::FeedOrdersByTheAcceptanceInstant
            | Defect::ReconciliationSummaryIgnoresWhetherTheFoldAccrues
            | Defect::SkipsRowsRatherThanSeeksTheBoundaryKey => Self::Declared,
        }
    }

    /// The row this order picks out of a bucket, or `None` when the bucket is
    /// empty.
    fn pick<'rows>(
        self,
        rows: &'rows [&StoredUsageRecord],
    ) -> Option<&'rows &'rows StoredUsageRecord> {
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

/// The accrued total over the selection, for the `SUM` branch of
/// [`QuantitySummary`], mirroring the reference backend's `accrued_sum`.
fn accrued_sum(rows: &[&StoredUsageRecord]) -> Result<BigDecimal, UsageCollectorPluginError> {
    let mut total = BigDecimal::from(0);
    for row in rows {
        total += widen(row.quantity.as_decimal())?;
    }
    Ok(total)
}
