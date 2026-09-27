//! Tests for the contract suite itself.
//!
//! Five different things are asserted here, in the order the file puts
//! them, and none is a plugin's conformance.
//!
//! The first is that the suite runs and passes against a backend built to
//! conform, which is what makes a violation reported against a real plugin
//! worth reading.
//!
//! The second is that the checks *discriminate*. The reference backend and
//! the assertions were written alongside each other, so the first assertion
//! establishes that the suite **runs** and nothing about whether any check
//! would notice a non-conforming plugin — and a check that cannot fail is
//! worse than a missing one, because a port is accepted on it and it reads
//! as coverage. [`super::contract_mutants`] holds twenty-three deliberately
//! non-conforming subjects, each behaviourally the reference backend wrong
//! in exactly one plausible way, and
//! [`each_check_fails_against_its_own_defect_and_no_other`] asserts a whole
//! column against each of them. That test runs every subject at
//! [`DedupLevel::Linearizable`];
//! [`an_undecided_lookup_is_still_a_violation_once_the_bound_has_passed`]
//! and
//! [`a_race_that_left_two_writes_is_a_violation_once_the_bound_has_passed`]
//! are the
//! two columns asserted at the other declaration, because two checks have
//! paths only an `Eventual` declaration reaches.
//!
//! The third is that the three coverage constants still partition DESIGN's
//! sixteen checks, so a passing run cannot read as a complete one.
//!
//! The fourth is the fixture vocabulary's own two guards: that every check
//! name derives a valid, distinct meter, and that the tenant factory cannot
//! mint one of the four named tenant ids. Both keep one check's entries out
//! of another's reads on the single shared backend `run_all` dispatches
//! against — a premise no check can assert for itself, because breaking it
//! decides a check on dispatch order rather than on the plugin.
//!
//! The fifth is the reference backend's own fail-closed posture, which is
//! not a contract check but is the thing a plugin author copies.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use bigdecimal::BigDecimal;
use toolkit_odata::{ODataQuery, ast};
use uuid::Uuid;

use super::contract_mutants::{Defect, mutant};
use super::{
    ADDITIONAL_CHECKS, AT_MOST_ONE_INVALIDATION, BLOCKED_CHECKS, CONVERGED_TARGET_LOOKUP,
    DEDUP_CONCURRENT, DEDUP_FLOOR, DEDUP_IDENTITY_OVER_WINDOW, DedupLevel, FEED_COMPLETENESS,
    FEED_SNAPSHOT_AND_REPLAY, HARNESS_FAULT, IMPLEMENTED_CHECKS, INVALIDATION_EXCLUDED_FROM_FOLD,
    LATEST_TIE_BREAK, QUANTITY_ROUND_TRIP, RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
    SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH, SERVER_FIELD_ROUND_TRIP, UNWRITTEN_CHECKS,
    WINDOW_END_SELECTION, reference::InMemoryReferencePlugin, retention::ContractRetention,
    run_all,
};
use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPage, FeedPosition, FeedStart};
use crate::models::{
    AggregationFold, CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, RecordOrigin,
    ResourceRef, UsageRecord,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

#[tokio::test]
async fn the_reference_backend_conforms() {
    let plugin = InMemoryReferencePlugin::new();

    let violations = run_all(&plugin, DedupLevel::Linearizable).await;

    assert!(
        violations.is_empty(),
        "the reference backend is the suite's own subject and must pass every implemented \
         check; it reported: {violations:#?}"
    );
}

/// The same run under [`DedupLevel::Eventual`], which is the only declaration
/// that reaches `at-most-one-invalidation`'s post-convergence ledger read and
/// `COUNT` fold. A backend that decides every write as it commits satisfies
/// the weaker declaration too, with a zero bound.
#[tokio::test]
async fn the_reference_backend_conforms_under_an_eventual_declaration() {
    let plugin = InMemoryReferencePlugin::new();

    let violations = run_all(
        &plugin,
        DedupLevel::Eventual {
            convergence_bound: std::time::Duration::ZERO,
        },
    )
    .await;

    assert!(
        violations.is_empty(),
        "the reference backend must pass every implemented check under an `Eventual` \
         declaration; it reported: {violations:#?}"
    );
}

/// The suite run three times against **one** backend, which is the shape a
/// real porter's second `cargo test` takes.
///
/// This module's header states the premise — *"The suite writes entries and
/// never removes them, so a backend under test starts each run from whatever
/// state the previous one left; the fixtures are keyed so that a repeated run
/// resubmits identical entries rather than colliding with different ones"* —
/// and until this test nothing held it. Every check depends on it, and each
/// depends on it differently: one that counts rows needs its own re-delivery
/// to be absorbed rather than stored, and one that asserts an entry is
/// *accepted* needs that assertion to admit an absorb, because on the second
/// run the entry is already there.
///
/// The failure it catches is a check written against a first run alone —
/// "this is the first entry of its identity, so it is accepted" — which is
/// true once and false afterwards. Three runs rather than two, because the
/// first repeat is the one a fixture keyed on a run counter would survive.
#[tokio::test]
async fn the_reference_backend_conforms_to_a_repeated_run() {
    let plugin = InMemoryReferencePlugin::new();

    for run in 1..=3 {
        let violations = run_all(&plugin, DedupLevel::Linearizable).await;
        assert!(
            violations.is_empty(),
            "run {run} of the suite against one backend that kept the previous runs' entries \
             reported: {violations:#?}. The suite removes nothing, so every check has to be \
             written for a store that already holds its fixtures: a re-delivery is absorbed \
             rather than stored, and an assertion that an entry is accepted has to admit that \
             absorb."
        );
    }
}

/// One row per defect: the subject, and the checks `run_all` must report
/// against it.
///
/// The check names are the exported constants rather than string literals.
/// A literal here would go on matching a constant that had been respelled,
/// and the row would then assert nothing about the check it names.
///
/// **Seven rows name more than one check**, and none is a mutant wrong
/// twice: each is a real overlap between checks, which is what the matrix
/// has to be able to say without loosening into a subset assertion.
///
/// [`Defect::SelectsOnWindowStart`] is the first. `quantity-round-trip`
/// reads its entries back over a range around each entry's `window_end`,
/// and its fixtures start an hour earlier, so a backend selecting on
/// `window_start` returns none of them and the check reports that it could
/// not compare a quantity at all. The dependency is the suite's, not the
/// subject's — the quantity check cannot be answered by a backend that
/// fails period-end selection — so the row names both.
///
/// [`Defect::DedupIgnoresTheEntryType`] is the second, and the suite
/// predicted it before the subject existed: `dedup-identity-over-window`'s
/// module says of a dedup blind to the entry type that it *"fails
/// `at-most-one-invalidation` and `invalidation-excluded-from-fold` too,
/// both submitting the record first and so having its withdrawal refused"*.
/// Five checks submit a record and then an invalidation repeating its key,
/// and an index blind to the sixth identity input refuses the second of
/// every such pair: `invalidation-excluded-from-fold` loses the withdrawal
/// that makes its withdrawn pair, `at-most-one-invalidation` loses the
/// withdrawals of both its targets, `server-field-round-trip` loses the
/// only entry in its fixture set that carries an `invalidates` to read back
/// at all, and `feed-completeness` loses the whole of its second ingestion
/// round — so none of them reaches the property it exists for. That is one
/// mistake meeting five checks, not five mistakes, so the row names all
/// five.
///
/// The fourth and fifth are coupled unavoidably rather than incidentally,
/// which is why this is an overlap and not a subject wrong twice.
/// `server-field-round-trip` has to read `invalidates`, a plain record
/// carries none, and an invalidation repeats its target's idempotency key by
/// construction (DESIGN §3.1, Faithful copy) — so every fixture set able to
/// assert that field at all hands an entry-type-blind index a collision.
/// `feed-completeness` is the same argument for a different field: DESIGN's
/// row for it is the Feed order invariant *"under concurrent ingestion of
/// records **and invalidations**"*, and its correction-order assertion is
/// about a withdrawal and the record it withdraws, so a fixture set without
/// one asserts nothing. There is no arrangement that separates either from
/// this subject.
///
/// What the wider row costs is worth saying plainly: it establishes that
/// these four checks together notice an entry-type-blind plugin, not which
/// of them noticed. [`Defect::ConflictReadBackIgnoresTheEntryType`] is the
/// row that answers that, and it is why the two sit beside each other.
/// DESIGN's obligation names three places `entry_type` has to appear, and
/// the two subjects strike it out of one each: the unique constraint, where
/// an entry is refused and every check needing that pair loses its
/// fixtures, and the read-back of a conflicting entry, where nothing is
/// refused and only the answer to a retry changes. The second isolates to
/// `record-and-invalidation-distinct-identity` alone, and to the one
/// assertion in it that no other check makes — that a retry of a withdrawn
/// record is absorbed against the record rather than against the
/// invalidation sharing its five caller-supplied components.
///
/// [`Defect::LedgerHasNoUniqueConstraint`] is the third. It decides every
/// collision by reading the ledger and then writes regardless, so every
/// outcome it returns is the conforming one and a second row lands beside
/// the first under every identity that is submitted twice. Three checks
/// re-deliver an entry and then count what a range comes back with:
/// `dedup-identity-over-window`'s second half counts the rows carrying one
/// id, `record-and-invalidation-distinct-identity` counts a record and its
/// withdrawal after a retry of the record, and `dedup-floor` counts one row
/// per identity on the ledger page and one term per identity in a `COUNT`
/// fold. One mistake meeting three checks, not three mistakes.
///
/// The overlap is unavoidable rather than incidental, which is what makes it
/// an overlap. "One identity reads at most once" is a property of the store
/// and not of any answer, so the only way to assert it is to submit an
/// identity twice and count the rows — and every check that does that is a
/// check this subject fails. What the wider row costs is the same thing the
/// row above costs: it establishes that these three together notice a ledger
/// with no unique constraint, not which of them noticed. What `dedup-floor`
/// adds over the other two, and what this row leaves untouched, is stated on
/// the defect itself and was measured rather than claimed.
///
/// [`Defect::BatchResolvesAgainstThePreCallLedger`] is the fourth, and its
/// two checks assert one rule for two kinds of entry. A batch whose rows are
/// all decided against the ledger as it stood before the call reports an
/// acceptance for the later of any same-identity pair inside it, and two
/// checks send such a pair: `dedup-floor`'s divergent in-batch pair of
/// records, and `at-most-one-invalidation`'s one-batch pair of withdrawals of
/// one target. DESIGN states the rule once, for entries rather than for
/// records or withdrawals, so a subject missing the in-batch dedup map meets
/// it wherever it is asserted. There is no fixture arrangement that would
/// separate them: the coupling is the rule's, not the subject's.
///
/// [`Defect::IgnoresScopeOnThePointRead`] is the fifth, and DESIGN puts both
/// its checks on the point read itself. `scope-is-a-filter-on-every-read-path`
/// exists for the obligation the SPI states on `get_usage_record` — *"`scope`
/// is the compiled PDP scope … A row outside it is absent"* — and
/// `converged-target-lookup` is obliged to read an out-of-scope entry too:
/// its DESIGN row names *"an out-of-scope entry, which answers as an absent
/// one"*, and the obligation behind that row opens *"A lookup with
/// `converged_only` applies `scope` first"*. A subject that drops the scope
/// argument on this method therefore meets both, and it meets the second
/// twice over — the converged-only read and the caller-facing one, which is
/// the probe that establishes the flag governs convergence and never
/// authorization. One mistake meeting two checks, not two mistakes: the
/// coupling is DESIGN's, which states the scope rule for this method once and
/// then obliges the converged-only lookup to apply it first.
///
/// What the wider row costs is what the others cost: it establishes that the
/// two checks together notice a point read that drops its scope, not which of
/// them noticed. Neither is separable by any fixture arrangement — both have
/// to store a row outside the scope they dispatch and ask for it by `id`, and
/// there is one method to ask.
///
/// Inside `converged-target-lookup` the row rests on **two** assertions and
/// on neither alone, measured by neutering each: that subject answers the
/// withheld row under both flags, so the converged-only probe and the
/// caller-facing one each report on their own and taking either away leaves
/// the row unchanged. Taking both away removes this check from the row and
/// changes nothing else.
///
/// [`Defect::StampsItsOwnAcceptedAt`] is the sixth, and the newest: it
/// arrived at a second check when `latest-tie-break` landed. That subject
/// writes one fixed instant into `accepted_at` on admission, so any two
/// entries it stores agree on the middle key of DESIGN §3.1's `LATEST`
/// order — and that check's `accepted_at` scenario is two entries that agree
/// on the first key and are separated by the middle one. With the middle key
/// flattened the fold falls to `id`, which that scenario deliberately
/// inverts, so the wrong entry wins.
///
/// This coupling is DESIGN's too, and it is the tightest of the six. §3.1's
/// "Server-assigned field fidelity" obliges a plugin to persist the
/// `accepted_at` it was handed, and §3.1's `LATEST` tie-break reads that same
/// field as its middle key. A backend that does not keep the value cannot
/// order by it, so no fixture arrangement separates the two checks: one is
/// about storing the field and the other about ranking on it, and the second
/// presupposes the first. `latest-tie-break`'s other two scenarios pass
/// against this subject, which is the evidence the coupling is confined to
/// the one key — the `window_end` scenario separates above it and the
/// cross-tenant scenario ties on it by construction.
///
/// The three subjects that follow it in the matrix are the isolating ones
/// for that check: one per key of DESIGN's three-key order, each naming
/// `latest-tie-break` alone. `super::contract_mutants`'s header says why the
/// set of three is complete.
///
/// [`Defect::AFeedPageRedeliversTheEntryAtItsCursor`] is the seventh and the
/// last, and it joined a second check when `feed-completeness` landed. A
/// page that resumes *at* the entry its start position names rather than
/// after it re-delivers that entry on every page but the first, and the two
/// checks see the one mistake from two directions: the paginated scan in
/// `feed-snapshot-and-replay` reports an entry it had already passed
/// appearing again, and `feed-completeness` reports a delivery that carries
/// an entry more than once. Neither is separable by any fixture
/// arrangement — "each settled entry is handed to a consumer once" and "a
/// scan observes no entry appearing" are two readings of one deterministic
/// order, and a check able to assert either over a paginated walk is a check
/// this subject fails.
///
/// The two rows after it are the isolating ones for `feed-completeness`:
/// [`Defect::AFeedPageDropsTheEntryAtItsLimit`] for completeness and
/// [`Defect::FeedOrdersByTheAcceptanceInstant`] for correction order, each
/// naming that check alone. `super::contract_mutants`'s header says which
/// decision of a feed page each of the five feed subjects lands in.
const DISCRIMINATION_MATRIX: &[(Defect, &[&str])] = &[
    (Defect::QuantityThroughFloat, &[QUANTITY_ROUND_TRIP]),
    (
        Defect::StampsItsOwnAcceptedAt,
        &[SERVER_FIELD_ROUND_TRIP, LATEST_TIE_BREAK],
    ),
    (Defect::DefaultsOriginToLive, &[SERVER_FIELD_ROUND_TRIP]),
    (
        Defect::SelectsOnWindowStart,
        &[WINDOW_END_SELECTION, QUANTITY_ROUND_TRIP],
    ),
    (Defect::DedupIgnoresThePeriod, &[DEDUP_IDENTITY_OVER_WINDOW]),
    (
        Defect::DedupIgnoresTheEntryType,
        &[
            RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
            INVALIDATION_EXCLUDED_FROM_FOLD,
            AT_MOST_ONE_INVALIDATION,
            SERVER_FIELD_ROUND_TRIP,
            FEED_COMPLETENESS,
        ],
    ),
    (
        Defect::ConflictReadBackIgnoresTheEntryType,
        &[RECORD_AND_INVALIDATION_DISTINCT_IDENTITY],
    ),
    (
        Defect::FoldsTheInvalidation,
        &[INVALIDATION_EXCLUDED_FROM_FOLD],
    ),
    (
        Defect::AbsorbsAWithdrawalWithAnotherReason,
        &[AT_MOST_ONE_INVALIDATION],
    ),
    (
        Defect::RefusesAWithdrawalWithTheSameReason,
        &[AT_MOST_ONE_INVALIDATION],
    ),
    (
        Defect::IgnoresScopeOnThePointRead,
        &[
            SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
            CONVERGED_TARGET_LOOKUP,
        ],
    ),
    (
        Defect::AnswersNotConvergedForAnAcknowledgedEntry,
        &[CONVERGED_TARGET_LOOKUP],
    ),
    (
        Defect::LedgerHasNoUniqueConstraint,
        &[
            DEDUP_FLOOR,
            DEDUP_IDENTITY_OVER_WINDOW,
            RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
        ],
    ),
    (
        Defect::BatchResolvesAgainstThePreCallLedger,
        &[DEDUP_FLOOR, AT_MOST_ONE_INVALIDATION],
    ),
    (
        Defect::ADivergentWriteDisplacesTheSurvivor,
        &[DEDUP_CONCURRENT],
    ),
    (Defect::LatestSkipsTheAcceptanceInstant, &[LATEST_TIE_BREAK]),
    (
        Defect::LatestStopsAtTheAcceptanceInstant,
        &[LATEST_TIE_BREAK],
    ),
    (Defect::LatestIgnoresThePeriodEnd, &[LATEST_TIE_BREAK]),
    (
        Defect::FeedCursorCountsAdmittedEntries,
        &[FEED_SNAPSHOT_AND_REPLAY],
    ),
    (
        Defect::AFeedPageRedeliversTheEntryAtItsCursor,
        &[FEED_SNAPSHOT_AND_REPLAY, FEED_COMPLETENESS],
    ),
    (
        Defect::ABoundedReplayNeverCloses,
        &[FEED_SNAPSHOT_AND_REPLAY],
    ),
    (
        Defect::AFeedPageDropsTheEntryAtItsLimit,
        &[FEED_COMPLETENESS],
    ),
    (
        Defect::FeedOrdersByTheAcceptanceInstant,
        &[FEED_COMPLETENESS],
    ),
];

/// Every check fails against a backend that gets its rule wrong, and passes
/// against every other backend.
///
/// The second half is what makes this a test of *discrimination* rather than
/// of sensitivity. A check that fails against all twenty-three mutants is not
/// detecting its own rule; it is detecting that something is different. So
/// each row asserts a full column: the named check fails, and the others
/// still pass against the same mutant.
///
/// The subjects are in [`super::contract_mutants`], which also says why each
/// is built by wrapping the reference backend or by carrying a ledger of its
/// own, and why none of it is a switch inside `reference.rs`.
///
/// The closing assertion is against the checks the coverage constants
/// *declare* — `IMPLEMENTED_CHECKS` together with `ADDITIONAL_CHECKS` — not
/// against `run_all`'s body. A check added to the suite with no subject to
/// fail it would otherwise sit in the matrix's blind spot, which is the very
/// thing this test exists to take away.
///
/// The distinction is worth stating because it bounds what this catches. A
/// check added to `run_all` **and** to a coverage constant, with no mutant,
/// is caught here. One added to `run_all` and to neither constant escapes
/// this test and the partition test alike — nothing ties `run_all`'s call
/// list to the constants, and that gap predates the matrix.
#[tokio::test]
async fn each_check_fails_against_its_own_defect_and_no_other() {
    let mut named: BTreeSet<&str> = BTreeSet::new();
    for (defect, expected) in DISCRIMINATION_MATRIX {
        let plugin = mutant(*defect);
        let failed: BTreeSet<&str> = run_all(plugin.as_ref(), DedupLevel::Linearizable)
            .await
            .into_iter()
            .map(|violation| violation.check)
            .collect();
        let expected: BTreeSet<&str> = expected.iter().copied().collect();
        named.extend(expected.iter().copied());

        assert_eq!(
            failed, expected,
            "the `{defect:?}` subject is behaviourally the reference backend wrong in exactly one \
             way, and `run_all` must report exactly the checks that rule belongs to. A check \
             missing from the reported set cannot catch the mistake it exists for; an extra one \
             is either a subject wrong in a second way or a check detecting difference rather \
             than its own rule, and both make the suite read as coverage it does not have."
        );
    }

    let declared_by_the_coverage_constants: BTreeSet<&str> = IMPLEMENTED_CHECKS
        .iter()
        .chain(ADDITIONAL_CHECKS)
        .copied()
        .collect();
    assert_eq!(
        named, declared_by_the_coverage_constants,
        "every check `IMPLEMENTED_CHECKS` and `ADDITIONAL_CHECKS` declare must be named by some \
         row of the discrimination matrix, and the matrix must name no check they do not declare. \
         A declared check with no subject built to fail it is a check nothing here establishes \
         anything about. This is a statement about the coverage constants, not about `run_all`'s \
         call list: a check added to `run_all` and to neither constant escapes this assertion, \
         and see this test's doc for why that gap is not this test's to close."
    );
}

/// An undecided converged-only lookup is still a violation once the declared
/// bound has passed.
///
/// **This is the only test that executes `converged-target-lookup`'s second
/// read**, and it exists for that reason. That check's first probe admits
/// `UsageRecordNotConverged` under [`DedupLevel::Eventual`], sleeps the
/// declared convergence bound and reads again; nothing else here takes the
/// branch, because the reference backend answers the survivor straight away
/// under either declaration and
/// [`each_check_fails_against_its_own_defect_and_no_other`] runs every
/// subject at [`DedupLevel::Linearizable`].
///
/// [`Defect::AnswersNotConvergedForAnAcknowledgedEntry`] is a backend that
/// never decides, so the sleep runs and the second read answers undecided
/// again. The check must still report: DESIGN §3.3 admits the answer *"only
/// until it can decide"*, and a lookup that never leaves it is a caller
/// retrying forever. Asserting the whole failing set rather than just that
/// this check is in it keeps the test honest about the level, too — an
/// `Eventual` declaration is also the only one that reaches
/// `at-most-one-invalidation`'s post-convergence half, and this subject must
/// pass that.
///
/// The bound is zero, so the test does not actually wait. What is being
/// established is that the branch runs and still reports, not how long it
/// waits for — no check in this suite times a plugin.
#[tokio::test]
async fn an_undecided_lookup_is_still_a_violation_once_the_bound_has_passed() {
    let plugin = mutant(Defect::AnswersNotConvergedForAnAcknowledgedEntry);

    let failed: BTreeSet<&str> = run_all(
        plugin.as_ref(),
        DedupLevel::Eventual {
            convergence_bound: std::time::Duration::ZERO,
        },
    )
    .await
    .into_iter()
    .map(|violation| violation.check)
    .collect();

    assert_eq!(
        failed,
        BTreeSet::from([CONVERGED_TARGET_LOOKUP]),
        "a backend that answers `UsageRecordNotConverged` for an entry it has already \
         acknowledged must fail `converged-target-lookup` under an `Eventual` declaration too, \
         and fail nothing else. Under that declaration the check waits out the whole of the \
         declared convergence bound before it requires an answer, which is the one path through \
         it no other test in this file takes; the answer is still undecided afterwards, and \
         undecided is admissible only until the plugin can decide."
    );
}

/// A race that left two writes on one identity is still a violation once
/// the declared bound has passed.
///
/// **This is the only test that executes `dedup-concurrent`'s `Eventual`
/// half**, and it exists for that reason. Under a `linearizable`
/// declaration that check asserts the two outcomes of a divergent race and
/// reads the survivor's content back; under an `eventual` one it asserts
/// none of that, because DESIGN admits an acknowledgement there that is
/// later discarded. What it asserts instead is what the declaration still
/// owes after the bound: *"only the first write in commit order ever shows
/// on any read, figure, or feed page"*, over a `list_usage_records` page, a
/// `COUNT` fold and a feed page.
///
/// [`each_check_fails_against_its_own_defect_and_no_other`] runs every
/// subject at [`DedupLevel::Linearizable`], so without this the three
/// surfaces would run against the reference backend alone — green, and
/// establishing only that they do not false-positive.
/// [`Defect::LedgerHasNoUniqueConstraint`] is the subject that makes them
/// discriminate: it decides every collision the conforming way and writes
/// regardless, so both writes of each raced identity land and all three
/// surfaces show two where one is owed.
///
/// The subject built for `dedup-concurrent` itself,
/// [`Defect::ADivergentWriteDisplacesTheSurvivor`], is deliberately **not**
/// the one used here: it replaces a row in place, so every surface goes on
/// showing exactly one write per identity and agreeing on which, and it
/// fails nothing at all under this declaration. That was measured, and it
/// is recorded on the defect.
///
/// Asserting the whole failing set rather than just that this check is in
/// it keeps the test honest about the level. An `Eventual` declaration is
/// also the only one that reaches `at-most-one-invalidation`'s
/// post-convergence half, which this subject fails too — it stores each
/// withdrawal twice — and the four checks it fails at `Linearizable` must
/// go on failing here and no others must join them.
///
/// The bound is zero, so the test does not actually wait. What is being
/// established is that the three surfaces run and still report, not how
/// long they wait for — no check in this suite times a plugin.
#[tokio::test]
async fn a_race_that_left_two_writes_is_a_violation_once_the_bound_has_passed() {
    let plugin = mutant(Defect::LedgerHasNoUniqueConstraint);

    let failed: BTreeSet<&str> = run_all(
        plugin.as_ref(),
        DedupLevel::Eventual {
            convergence_bound: std::time::Duration::ZERO,
        },
    )
    .await
    .into_iter()
    .map(|violation| violation.check)
    .collect();

    assert_eq!(
        failed,
        BTreeSet::from([
            DEDUP_CONCURRENT,
            DEDUP_FLOOR,
            DEDUP_IDENTITY_OVER_WINDOW,
            RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
            AT_MOST_ONE_INVALIDATION,
        ]),
        "a backend whose dedup identity carries no unique constraint stores both writes of a \
         raced identity, and after the declared convergence bound `dedup-concurrent` must report \
         it: a ledger page, a `COUNT` fold and a feed page each show two writes where only the \
         first in commit order may show. The other four are this subject's `Linearizable` row \
         plus `at-most-one-invalidation`, whose post-convergence half only an `Eventual` \
         declaration reaches and which this subject fails by storing each withdrawal twice. A \
         check joining this set is a subject wrong in a second way; one leaving it is an \
         assertion that stopped discriminating."
    );
}

/// Nothing is listed as beyond the SPI's reach that this suite implements,
/// that it also calls merely unwritten, or that carries a justification
/// already known to be false — and, today, nothing is listed at all.
///
/// `BLOCKED_CHECKS` says *cannot express* rather than *not implemented*,
/// and that is a claim about the shape of [`UsageCollectorPluginV1`] rather
/// than about this crate's progress. **No test can check it.** A trait's
/// method set is not reachable at runtime on stable Rust, and the claim is
/// not even always about a method: of the two entries this constant used to
/// hold, one was blocked on a missing SPI method (`read_feed_page`, which
/// the SPI now declares) and the other on a rule in DESIGN's prose that
/// read an `acceptance_sequence` field the record does not carry — a rule
/// §3.1 has since settled differently, so the check became writable with
/// no field ever added. A guard that reflected over the trait would have
/// caught the first and passed the second.
///
/// So this test does three things it can do and names the one it cannot.
///
/// The structural assertions come first, and each arms itself the moment an
/// entry is added: a blocked check must not be in `IMPLEMENTED_CHECKS` (a
/// check that landed and stayed listed under-reports coverage), must not
/// also be in `UNWRITTEN_CHECKS` (the two make incompatible claims about
/// the same name), must carry a reason, and must not carry one of
/// `RETIRED_JUSTIFICATIONS` — the two this constant was caught holding,
/// both provably false and both cheap to resurrect from an old commit.
///
/// The emptiness assertion comes last, deliberately, so an entry that is
/// structurally wrong is diagnosed as wrong rather than merely as new. It
/// is a stop sign, not a snapshot. The previous version of this test
/// asserted `BTreeSet::from(["feed-snapshot-and-replay",
/// "latest-tie-break"])` and its own doc claimed to catch "a row that
/// stayed here after its check became writable" — yet both rows went
/// stale underneath it and it passed, because it pinned the *names* while
/// the rot was in the *reasons*. Emptiness makes no claim that can rot
/// that way. Its whole job is to stop the next author, who must then
/// establish by hand, and record in the entry, that:
///
/// 1. no method on [`UsageCollectorPluginV1`] lets the check be written,
///    naming the method that would;
/// 2. the DESIGN rule the check asserts still needs the thing the SPI
///    lacks — the trap the `latest-tie-break` row fell into;
/// 3. `UNWRITTEN_CHECKS` is not the honest home for it instead.
///
/// A limitation named beats a guard that cannot fire, which is exactly what
/// the hardcoded set was.
#[test]
fn the_blocked_checks_are_the_ones_the_spi_cannot_express() {
    /// Justifications this constant has already been caught holding after
    /// they stopped being true. Matched as substrings of a reason, so a
    /// reworded revival is caught with the original.
    ///
    /// **Substring matching on prose over-rejects, and that is the accepted
    /// trade.** A future entry that mentions `acceptance_sequence` for some
    /// unrelated and entirely legitimate reason fails this assertion too.
    /// The cost is bounded to a reword because of where the guard sits: it
    /// can only fire after someone has deliberately pushed past the
    /// emptiness stop sign below, so a false positive lands on an author who
    /// is already editing this test and reading this doc, not on a passer-by
    /// — and the failure is loud rather than a silently corrupted claim. If
    /// you are that author: say the same thing without the retired phrase,
    /// or drop the phrase from this list with a note saying why it can no
    /// longer mislead.
    const RETIRED_JUSTIFICATIONS: [&str; 2] = ["no feed method", "acceptance_sequence"];

    let blocked: BTreeSet<&str> = BLOCKED_CHECKS.iter().map(|(check, _)| *check).collect();

    for check in IMPLEMENTED_CHECKS {
        assert!(
            !blocked.contains(check),
            "`{check}` is implemented and run by `run_all`, so listing it as blocked would \
             under-report the suite's coverage"
        );
    }
    for check in UNWRITTEN_CHECKS {
        assert!(
            !blocked.contains(check),
            "`{check}` is named as blocked and as unwritten, which are incompatible claims: one \
             says the SPI cannot express the check, the other that it can and nobody has written \
             it. A reader cannot tell which is meant, and the partition test cannot tell either"
        );
    }
    for (check, reason) in BLOCKED_CHECKS {
        assert!(
            !reason.trim().is_empty(),
            "`{check}` is listed as blocked with no reason: the entry exists to say what \
             unblocks it, and an empty one only hides the check"
        );
        for retired in RETIRED_JUSTIFICATIONS {
            assert!(
                !reason.contains(retired),
                "`{check}` is blocked on `{retired}`, which was true once and is not now. The \
                 SPI declares `read_feed_page`, and the DESIGN section 3.1 order reads \
                 `window_end`, `accepted_at` and `id` rather than an `acceptance_sequence` the \
                 record never carried. Both of these justifications were held here after they \
                 became false; neither is a blocker again without the SPI changing back"
            );
        }
    }

    assert!(
        blocked.is_empty(),
        "`BLOCKED_CHECKS` names {blocked:?}, and this assertion exists to stop you here. Nothing \
         automated can confirm that a check is beyond the SPI's reach: the trait's method set is \
         not visible at runtime, and the last two entries here went stale without a single test \
         failing. Before changing this assertion, establish by hand that no method on \
         `UsageCollectorPluginV1` lets the check be written, that the DESIGN rule it asserts \
         still needs what the SPI lacks, and that `UNWRITTEN_CHECKS` is not the honest home for \
         it. Then say which of those you checked, in the entry"
    );
}

/// Every check DESIGN §3.3 declares is implemented, blocked, or named as
/// unwritten — exactly once, and nothing else is any of the three.
///
/// This is what makes the module's coverage claim structural instead of
/// narrative. `run_all` returning no violations says nothing about a check
/// it never ran, and "run this suite" is the acceptance criterion for
/// porting a storage backend, so a suite that runs ten checks must not
/// read as a suite that ran sixteen. Asserting the partition means a check
/// cannot half-land — implemented but still listed unwritten, or written
/// and listed nowhere — without this failing, and `UNWRITTEN_CHECKS`
/// empties itself as the work lands rather than needing someone to
/// remember.
///
/// `ADDITIONAL_CHECKS` is deliberately outside the partition and asserted
/// against it rather than folded into it. `run_all` runs a check DESIGN
/// does not tabulate, and admitting it to `IMPLEMENTED_CHECKS` would force
/// the equality below down to a subset check — which no longer catches a
/// DESIGN check written and listed nowhere, the exact failure the partition
/// exists for. Held disjoint instead, the fourth constant cannot become a
/// place to park a DESIGN name to escape the accounting.
#[test]
fn the_three_coverage_constants_partition_the_design_checks() {
    /// The sixteen names in DESIGN §3.3's "Plugin contract tests" table.
    const DESIGN_CHECKS: [&str; 16] = [
        "window-end-selection",
        "invalidation-excluded-from-fold",
        "at-most-one-invalidation",
        "record-and-invalidation-distinct-identity",
        "converged-target-lookup",
        "dedup-identity-over-window",
        "dedup-floor",
        "dedup-concurrent",
        "quantity-round-trip",
        "server-field-round-trip",
        "feed-snapshot-and-replay",
        "feed-completeness",
        "feed-bootstrap-position",
        "feed-retention-refusal",
        "feed-position-bounded",
        "latest-tie-break",
    ];

    let mut claimed: Vec<&str> = IMPLEMENTED_CHECKS.to_vec();
    claimed.extend_from_slice(UNWRITTEN_CHECKS);
    claimed.extend(BLOCKED_CHECKS.iter().map(|(check, _)| *check));

    let unique: BTreeSet<&str> = claimed.iter().copied().collect();
    assert_eq!(
        unique.len(),
        claimed.len(),
        "a check is named in more than one coverage constant, so the three do not partition \
         anything: {claimed:?}"
    );
    assert_eq!(
        unique,
        BTreeSet::from(DESIGN_CHECKS),
        "the implemented, unwritten and blocked constants must together be exactly DESIGN \
         section 3.3's sixteen checks: no invented name, and nothing left unaccounted for"
    );
    assert!(
        !unique.contains(HARNESS_FAULT),
        "`{HARNESS_FAULT}` marks a fault in the suite rather than a DESIGN check, so it must \
         never appear in the coverage constants"
    );

    for check in ADDITIONAL_CHECKS {
        assert!(
            !unique.contains(check),
            "`{check}` is named in `ADDITIONAL_CHECKS`, which is for the checks DESIGN section \
             3.3 does not tabulate, and it also appears in the three constants that partition \
             DESIGN's sixteen. One of the two is wrong: either the name belongs in the partition \
             and not here, or the partition has grown a name DESIGN never wrote"
        );
    }
    assert!(
        ADDITIONAL_CHECKS.contains(&SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH),
        "`run_all` runs the scope check, so leaving it out of `ADDITIONAL_CHECKS` would make a \
         caller reporting coverage under-report what the run actually covered"
    );
}

/// Every check name derives a valid meter, and no two derivations collide.
///
/// The union of the three constants iterated below is invariant as a check
/// moves from `UNWRITTEN_CHECKS` to `IMPLEMENTED_CHECKS`, which is the only
/// move being made — `the_three_coverage_constants_partition_the_design_checks`
/// holds `IMPLEMENTED_CHECKS`, `UNWRITTEN_CHECKS` and `BLOCKED_CHECKS` to
/// DESIGN's sixteen, and `ADDITIONAL_CHECKS` carries the one name DESIGN
/// does not tabulate. So this keeps covering all seventeen names as checks
/// land and `UNWRITTEN_CHECKS` empties. `BLOCKED_CHECKS` is left out of the
/// iteration because it is empty and a blocked check has no fixtures to
/// keep apart; a name moved into it would leave this test, which that
/// constant's own stop-sign assertion makes a deliberate act.
///
/// **`MeterTypeId::new` fails at runtime, inside whichever fixture called
/// `check_meter`.** Without this test, a check name the `gts-id` grammar
/// rejects reaches a plugin author as a harness fault reported against a
/// conforming backend; with it, it is a unit failure naming the check.
///
/// The distinctness half is the property `check_meter`'s docs call
/// structural. It is asserted here rather than left to the argument because
/// the argument rests on the partition test, and a reader of one has no way
/// to see the other.
#[test]
fn every_check_name_derives_a_distinct_valid_meter() {
    let mut derived: BTreeMap<String, (&str, &str)> = BTreeMap::new();

    for &check in IMPLEMENTED_CHECKS
        .iter()
        .chain(ADDITIONAL_CHECKS)
        .chain(UNWRITTEN_CHECKS)
    {
        for role in ["main", "busy", "quiet", "other"] {
            let meter = super::fixtures::check_meter(check, role).unwrap_or_else(|err| {
                panic!(
                    "`{check}` under role `{role}` must derive a valid meter, and a failure here \
                     is the `gts-id` grammar rather than the call site: {err}"
                )
            });
            if let Some((earlier, earlier_role)) =
                derived.insert(meter.as_str().to_owned(), (check, role))
            {
                panic!(
                    "`{check}`/`{role}` and `{earlier}`/`{earlier_role}` derive the one meter \
                     `{meter}`: `run_all` dispatches every check against one backend that never \
                     removes an entry, so two checks on one meter read each other's entries and \
                     what either observes turns on dispatch order"
                );
            }
        }
    }
}

/// The tenant factory cannot mint one of the four named tenant ids.
///
/// `fixtures`' second compile-time assertion already holds
/// `CONTRACT_TENANT_BLOCK` above all four, and this reads the property off
/// the factory rather than off the constant. The two are not one claim: the
/// assertion says where the block starts, and this says that what
/// `contract_tenant` actually hands out stays clear of the named ids, which
/// is what fails when the factory stops deriving from that block at all.
///
/// The sharpest case either guard protects is `SCOPE_UNUSED_TENANT_ID`,
/// whose entire assertion value is that it owns no entry on a shared
/// backend that never removes one.
#[test]
fn the_tenant_factory_cannot_mint_a_named_tenant() {
    let named = [
        ("CONTRACT_TENANT_ID", super::fixtures::CONTRACT_TENANT_ID),
        (
            "SCOPE_EXCLUDED_TENANT_ID",
            super::fixtures::SCOPE_EXCLUDED_TENANT_ID,
        ),
        (
            "SCOPE_UNUSED_TENANT_ID",
            super::fixtures::SCOPE_UNUSED_TENANT_ID,
        ),
        (
            "FEED_OTHER_TENANT_ID",
            super::fixtures::FEED_OTHER_TENANT_ID,
        ),
    ];

    for index in 0..64_u32 {
        let minted = super::fixtures::contract_tenant(index);
        for (name, named_id) in named {
            assert_ne!(
                minted, named_id,
                "`contract_tenant({index})` minted `{name}`, so a check handed that tenant would \
                 share entries with the check that owns the named id"
            );
        }
    }
}

/// An uninterpretable comparison never admits a row, under `eq` or `ne`.
///
/// This is about the reference backend rather than about the contract, so
/// it is a unit test here and not a check in [`run_all`]: a plugin's own
/// scope projection is its business. The suite's own
/// `scope-is-a-filter-on-every-read-path` check asserts the obligation
/// every backend owes — a row outside the scope is withheld — and says
/// nothing about how a backend that cannot *read* part of a scope should
/// dispose of it, which is what this test pins for the exemplar.
///
/// The posture is load-bearing precisely *because* this backend is an
/// exemplar. Its module docs say a real plugin projects the same
/// obligations into SQL, so a hole here is a hole a plugin author inherits
/// — and the widening one is a cross-tenant read, not a cosmetic
/// over-match. The gear itself denies the very pairing exercised below: a
/// UUID-typed scope field against a value that is not a UUID lifts to
/// `AuthorizationDenied` in `authz::scope_value_to_ast`
/// (`usage-collector/src/domain/authz_tests.rs`). An exemplar more
/// permissive than the thing it models is the worst direction for the
/// error to run in.
///
/// `ne` is where this bites. `eq` folds "no match" and "cannot interpret"
/// into one exclusion harmlessly; `ne` inverts the answer, so a single
/// `bool` makes an unanswerable comparison *admit*. The scope
/// `Or(tenant_id eq <other tenant>, tenant_id ne "not-a-uuid")` names only
/// a tenant that does not own the row and still admitted it before
/// `value_matches` reported interpretability as an outcome of its own.
#[tokio::test]
async fn an_uninterpretable_comparison_admits_no_row_under_either_operator() {
    let plugin = InMemoryReferencePlugin::new();
    let record = scope_probe_record();
    let id = record.id;
    let other_tenant = Uuid::from_u128(0xdead_beef_0000_4000_8000_0000_0000_0002);
    plugin
        .create_usage_record(record)
        .await
        .expect("the reference backend admits a well-formed entry");

    // The reviewer's case, by name: a scope naming only another tenant,
    // widened back open by an `ne` against an operand no comparison can
    // read. This is a cross-tenant read in its most legible form.
    let cross_tenant = ast::Expr::Or(
        Box::new(compare(
            "tenant_id",
            ast::CompareOperator::Eq,
            ast::Value::Uuid(other_tenant),
        )),
        Box::new(compare(
            "tenant_id",
            ast::CompareOperator::Ne,
            ast::Value::String("not-a-uuid".to_owned()),
        )),
    );
    assert_not_found(
        &plugin,
        id,
        &cross_tenant,
        "a scope whose only satisfiable disjunct is an uninterpretable `ne` names no tenant that \
         owns this row, so admitting it is a cross-tenant read",
    )
    .await;

    // The same hole with no disjunction to hide behind: one node, no
    // tenant named at all, admitting every row in the ledger.
    assert_not_found(
        &plugin,
        id,
        &compare(
            "tenant_id",
            ast::CompareOperator::Ne,
            ast::Value::Bool(true),
        ),
        "`tenant_id ne <bool>` is a comparison this backend cannot make, so it must exclude \
         rather than match every row",
    )
    .await;

    // `eq` was already right, and stays right.
    assert_not_found(
        &plugin,
        id,
        &compare(
            "tenant_id",
            ast::CompareOperator::Eq,
            ast::Value::Bool(true),
        ),
        "an uninterpretable `eq` excludes the row",
    )
    .await;

    // The case the fix must not break: an absent attribute is unanswerable
    // through `record_field`, and `ne` over it stays `false`. The fixture
    // carries no `subject_ref`.
    assert_not_found(
        &plugin,
        id,
        &compare(
            "subject_id",
            ast::CompareOperator::Ne,
            ast::Value::String("someone".to_owned()),
        ),
        "`ne` against an attribute the row does not carry is SQL's NULL comparison: not selected",
    )
    .await;

    // The positive. An interpretable `ne` that genuinely does not match
    // still admits — without this, a fix that simply refused `ne` outright
    // would pass every assertion above while refusing valid scopes.
    let interpretable = ast::Expr::And(
        Box::new(compare(
            "tenant_id",
            ast::CompareOperator::Eq,
            ast::Value::Uuid(super::fixtures::CONTRACT_TENANT_ID),
        )),
        Box::new(compare(
            "resource_type",
            ast::CompareOperator::Ne,
            ast::Value::String("some.other.type".to_owned()),
        )),
    );
    let admitted = plugin
        .get_usage_record(id, &interpretable, false)
        .await
        .expect("an interpretable `ne` the row does not match must still admit it");
    assert_eq!(
        admitted.id, id,
        "the row admitted under an interpretable scope must be the row asked for"
    );
}

/// A `<identifier> <op> <literal>` node.
fn compare(field: &str, op: ast::CompareOperator, value: ast::Value) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier(field.to_owned())),
        op,
        Box::new(ast::Expr::Value(value)),
    )
}

/// Asserts a scope does not admit the row, which the SPI spells as
/// `UsageRecordNotFound` — never a distinguishable "denied".
async fn assert_not_found(
    plugin: &InMemoryReferencePlugin,
    id: Uuid,
    scope: &ast::Expr,
    why: &str,
) {
    let outcome = plugin.get_usage_record(id, scope, false).await;
    assert!(
        matches!(
            outcome,
            Err(UsageCollectorPluginError::UsageRecordNotFound { id: reported }) if reported == id
        ),
        "{why}"
    );
}

/// The subject-less row every scope above is evaluated against.
fn scope_probe_record() -> UsageRecord {
    CreateUsageRecord {
        entry_type: EntryType::Record,
        gts_type_id: MeterTypeId::new(super::fixtures::CONTRACT_METER_TYPE_ID)
            .expect("valid meter type id"),
        tenant_id: super::fixtures::CONTRACT_TENANT_ID,
        resource_ref: ResourceRef::new(
            super::fixtures::CONTRACT_RESOURCE_ID,
            super::fixtures::CONTRACT_RESOURCE_TYPE,
        )
        .expect("valid resource reference"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        quantity: UsageQuantity::parse("1").expect("fixture quantity"),
        idempotency_key: Some(IdempotencyKey::new("scope-probe").expect("valid idempotency key")),
        invalidation: None,
        window_start: super::fixtures::FIXTURE_EPOCH,
        window_end: super::fixtures::FIXTURE_EPOCH,
    }
    .try_into_usage_record(RecordOrigin::Live, super::fixtures::CONTRACT_ACCEPTED_AT)
    .expect("the probe fixture is projectable")
}

/// The same untranslatable node excludes as a scope and refuses as a filter.
///
/// These are opposite outcomes from one input shape, and that is the point.
/// The module's prose has always said *"an untranslatable predicate is a
/// refused query, never a dropped conjunct"* while the code answered an
/// empty selection for both, which is a silently wrong answer to a question
/// the caller did not ask. It is the `ne` defect in a second place: two
/// outcomes folded into one `false`.
///
/// The path is live rather than hypothetical. `toolkit-odata` parses
/// `contains`, and the gear's `reject_reserved_filter_fields` recurses
/// *through* `Expr::Function` rather than refusing it, then ANDs the
/// caller's raw expression onto the compiled scope before dispatch — so
/// `$filter=contains(resource_id,'x')` reaches `list_usage_records` today.
///
/// The scope half must stay an exclusion: refusing a lookup because a grant
/// could not be read would turn a fail-closed denial into a 500, and worse,
/// any weakening there is a cross-tenant read.
#[tokio::test]
async fn an_untranslatable_node_refuses_a_caller_filter_and_excludes_a_scope() {
    let plugin = InMemoryReferencePlugin::new();
    let record = scope_probe_record();
    let id = record.id;
    let meter = record.gts_type_id.clone();
    let window_end = record.window_end;
    plugin
        .create_usage_record(record)
        .await
        .expect("the reference backend admits a well-formed entry");

    // `contains(resource_id, 'contract')` — a function node, which this
    // backend cannot translate. It would match the row if it could.
    let function = ast::Expr::Function(
        "contains".to_owned(),
        vec![
            ast::Expr::Identifier("resource_id".to_owned()),
            ast::Expr::Value(ast::Value::String("contract".to_owned())),
        ],
    );

    // As a caller filter: refused, not answered with an empty page.
    let range = TimeRange::new(
        window_end,
        window_end.saturating_add(time::Duration::seconds(1)),
    )
    .expect("an ordered probe range");
    let query = ODataQuery::new().with_filter(function.clone());
    let refused = plugin
        .list_usage_records(meter, range, &query, &[])
        .await
        .expect_err("an untranslatable caller filter must refuse the read, not return no rows");
    assert!(
        matches!(refused, UsageCollectorPluginError::Internal(ref detail) if detail.contains("contains")),
        "the refusal must name the node kind it could not translate, so an operator can see \
         which predicate was rejected; got: {refused}"
    );

    // The very same node as a scope: excluded, and reported exactly as an
    // absent row so the surface stays free of an existence oracle.
    let excluded = plugin.get_usage_record(id, &function, false).await;
    assert!(
        matches!(
            excluded,
            Err(UsageCollectorPluginError::UsageRecordNotFound { id: reported }) if reported == id
        ),
        "a scope this backend cannot read must grant nothing and say so as `UsageRecordNotFound`, \
         never as a failure the caller could tell apart from an absent row; got: {excluded:?}"
    );

    // And the translatable filter still reads: the refusal is about the
    // node, not about filters in general.
    let translatable = ODataQuery::new().with_filter(compare(
        "resource_id",
        ast::CompareOperator::Eq,
        ast::Value::String(super::fixtures::CONTRACT_RESOURCE_ID.to_owned()),
    ));
    let page = plugin
        .list_usage_records(
            MeterTypeId::new(super::fixtures::CONTRACT_METER_TYPE_ID).expect("valid meter type id"),
            range,
            &translatable,
            &[],
        )
        .await
        .expect("a translatable filter must still be served");
    assert_eq!(
        page.items.len(),
        1,
        "the probe row matches the translatable filter, so refusing it would mean the fix had \
         over-reached from the untranslatable node to every filter"
    );
}

// ---------------------------------------------------------------------------
// The reference backend's feed page
// ---------------------------------------------------------------------------
//
// Unit tests of the reference implementation, not contract checks. DESIGN
// section 3.3's `feed-snapshot-and-replay` check is written against the SPI
// for any backend and belongs to a later slice; nothing below is added to
// `run_all` or to a coverage constant, because none of it is an obligation
// this suite puts on a plugin. What it is for is the suite's own subject:
// the reference backend's feed answers are what a plugin author reads as an
// exemplar, and they must not rot in the interval before the check that
// covers them arrives.

/// The tenant the feed reads below withhold.
///
/// Entries attributed to it are admitted by one grant below and withheld by
/// another, which is the only way a read can show that a position means the
/// same thing under either.
///
/// Minted in [`super::fixtures`] with the suite's other tenant ids, where one
/// compile-time assertion keeps them distinct. This constant was a second
/// `...0003` literal — the scope check's unused tenant — under a doc comment
/// claiming it was distinct from it, which is harmless only while no feed
/// check is in `run_all`: that suite shares one persistent backend across
/// every check, and the scope check's unused tenant asserts it owns no entry.
const FEED_OTHER_TENANT_ID: Uuid = super::fixtures::FEED_OTHER_TENANT_ID;

/// The ledger's length, and so the position at its end.
const FEED_LEDGER_LEN: u64 = 4;

/// Four entries in the ledger's append order, alternating the tenant the
/// grants below pin: A, B, A, B.
///
/// Interleaving is load-bearing rather than decorative. A backend that
/// counted admitted entries instead of scanned ones would still walk a
/// ledger whose entries were all admitted, and would still finish one whose
/// withheld entries were all at the end. Alternating them means a page's
/// cursor has to jump past an entry the page did not carry, every page.
///
/// Returns the backend and the entry ids in append order, so an assertion
/// names which entries a grant should have been handed rather than
/// re-deriving an identity.
async fn feed_ledger() -> (InMemoryReferencePlugin, Vec<Uuid>) {
    let plugin = InMemoryReferencePlugin::new();
    let tenants = [
        super::fixtures::CONTRACT_TENANT_ID,
        FEED_OTHER_TENANT_ID,
        super::fixtures::CONTRACT_TENANT_ID,
        FEED_OTHER_TENANT_ID,
    ];
    assert_eq!(
        u64::try_from(tenants.len()).unwrap_or(u64::MAX),
        FEED_LEDGER_LEN,
        "`FEED_LEDGER_LEN` is the position at this ledger's end and every assertion below reads \
         it, so it must be this ledger's own length"
    );

    let mut ids = Vec::new();
    for (index, tenant_id) in tenants.into_iter().enumerate() {
        // The idempotency key is one of the six inputs the derived identity
        // reads, so varying it alone makes four entries rather than one
        // entry replayed four times.
        let key = IdempotencyKey::new(format!("feed-position-{index}"))
            .expect("the feed fixture key is well formed");
        let offset = time::Duration::hours(i64::try_from(index).unwrap_or(0) + 1);
        let record = super::fixtures::fixture_record_for_tenant(
            tenant_id,
            &key,
            UsageQuantity::parse("1").expect("fixture quantity"),
            super::fixtures::FIXTURE_EPOCH,
            super::fixtures::FIXTURE_EPOCH.saturating_add(offset),
        )
        .expect("the feed fixture is projectable");
        ids.push(record.id);
        plugin
            .create_usage_record(record)
            .await
            .expect("the reference backend admits a well-formed entry");
    }
    (plugin, ids)
}

/// The meter [`feed_ledger`] writes every entry on, and the one a retention
/// drive below names.
fn feed_meter() -> MeterTypeId {
    MeterTypeId::new(super::fixtures::CONTRACT_METER_TYPE_ID)
        .expect("the suite's own meter type id is valid")
}

/// The subscription every feed read below dispatches: the one meter the
/// fixture vocabulary attaches every entry to.
fn feed_subscription() -> Vec<MeterTypeId> {
    vec![feed_meter()]
}

/// A compiled single-tenant grant: `tenant_id eq <tenant>`.
fn tenant_scope(tenant_id: Uuid) -> ast::Expr {
    compare(
        "tenant_id",
        ast::CompareOperator::Eq,
        ast::Value::Uuid(tenant_id),
    )
}

/// A grant this backend cannot translate at all.
///
/// A function node, the same shape
/// [`an_untranslatable_node_refuses_a_caller_filter_and_excludes_a_scope`]
/// dispatches. As a scope it admits nothing — a grant that cannot be read
/// grants nothing — which is the third, widest-apart admission the position
/// has to survive.
fn untranslatable_scope() -> ast::Expr {
    ast::Expr::Function(
        "contains".to_owned(),
        vec![
            ast::Expr::Identifier("resource_id".to_owned()),
            ast::Expr::Value(ast::Value::String("contract".to_owned())),
        ],
    )
}

/// A position as this backend spells one: a scanned-entry count in eight
/// big-endian bytes.
///
/// Spelled here rather than read back from the backend's private encoder, so
/// the expected value is a statement rather than an agreement with whatever
/// the subject produced.
fn feed_position(scanned: u64) -> FeedPosition {
    FeedPosition::new(scanned.to_be_bytes().to_vec())
        .expect("eight bytes is well inside the published position bound")
}

/// The entry ids a page carries, in the order it carried them.
fn entry_ids(entries: &[UsageRecord]) -> Vec<Uuid> {
    entries.iter().map(|entry| entry.id).collect()
}

/// One feed read that must succeed.
async fn feed_page(
    plugin: &InMemoryReferencePlugin,
    scope: &ast::Expr,
    start: FeedStart<FeedPosition>,
    until: Option<FeedPosition>,
    limit: u64,
) -> FeedPage<FeedPosition> {
    plugin
        .read_feed_page(&feed_subscription(), scope, start, until, limit)
        .await
        .expect("the reference backend serves a well-formed feed read")
}

/// One ledger, three grants, one meaning for a position.
///
/// This is the most valuable assertion in the file and the easiest to lose.
/// A feed position counts the entries a read **scanned**, not the ones it
/// **admitted**, so a position denotes a prefix of the ledger and denotes
/// the same prefix whoever reads it. The grant and the subscription decide
/// what a page *carries*, never what its cursor *counts* — which is what
/// DESIGN §3.1's `FeedPosition` row requires, fixing a position's age by the
/// oldest subsequent entry of a subscribed type *"whether or not the
/// reader's authorization scope admits that entry"*. That is what lets any
/// position be resumed under any grant without skipping an entry that grant
/// admits.
///
/// **The property is resumability, not identity.** Two reads under different
/// grants are not in general handed the same position: `limit` bounds the
/// entries a page admits while the scan is unbounded, so a page that stops
/// on the limit stops where its own grant's entries run out. Over this
/// ledger at `limit = 1` the two grants are handed 1 and 2 —
/// [`a_bounded_page_limit_hands_two_grants_two_positions_that_each_resume`]
/// is that read, and it is where the limit interaction is visible rather
/// than hidden. The three reads below all exhaust the ledger, which is the
/// case in which the positions do coincide; the coincidence is a
/// consequence of the exhausting limit and the property is what holds
/// without one.
///
/// Moving the scope gate above `cursor = entry.sequence` in `read_feed_page`
/// still compiles, still passes the entire contract suite — every feed check
/// is in `UNWRITTEN_CHECKS`, so `run_all` has nothing to say here — and still
/// reads correctly under every single grant. It would simply make a position
/// mean "the last entry this grant admitted", so a cursor minted under one
/// grant and resumed under a wider one silently skips every entry the
/// narrower grant withheld ahead of it.
///
/// **Measured, that defect fails four tests and no other.** It fails this one
/// at the first cross-grant equality below, where the two grants are handed 3
/// and 4; the fixpoint of
/// [`a_limit_bounded_feed_walk_reaches_a_fixpoint_at_the_ledger_end`], which
/// lands on 3 rather than the ledger's end; the absent cursor
/// [`a_bounded_feed_replay_closes_at_its_until_and_not_before`] requires,
/// which arrives as `Some(3)` so a bounded replay never reports completion;
/// and the literal in the single-grant
/// [`a_subscription_the_ledger_does_not_answer_still_advances_the_cursor`],
/// which is handed 0 rather than 4. Inside this test all three position
/// assertions fire, in order: 3 against 4, then 4 against the untranslatable
/// grant's 0, then 3 against the ledger's end.
///
/// The fifth guard is gone.
/// [`a_bounded_page_limit_hands_two_grants_two_positions_that_each_resume`]
/// caught this defect while a position was an offset and no longer does; its
/// own docs record why, and why it still earns its place.
///
/// **A position compared against a literal is still what pins the meaning,
/// though the cross-grant equalities now fire first.** They fire because a
/// sequence records *which* entry was counted last where an offset recorded
/// only *how many* were: two disjoint grants cannot share a last admitted
/// entry, so once each has admitted one their positions part company. Under
/// an offset both grants admit two of the four and both were handed 2, and
/// the equality passed. That makes the comparison sharper than it was and
/// still weaker than a literal, because it reports only that two positions
/// disagree and never which of them is right — a defect displacing every
/// grant's cursor alike would pass all three comparisons and be caught by the
/// literal alone. The cross-grant comparisons are kept for what they say when
/// they fail: they name two callers and one ledger, which is the form the
/// defect takes in a gateway.
///
/// **A sequence does not make the defect safer, only differently shaped.**
/// Under either mechanism a widened grant resuming from the cursor skips
/// every entry the narrow grant withheld ahead of its last admitted one —
/// here the ledger's second — and that silent loss is what this property
/// exists to prevent. What the offset added was a second fault: a cursor
/// counting admissions names a prefix by *length*, so a resumption lands
/// where the read never stood, and in the `limit = 1` walk the minting grant
/// was handed the ledger's third entry twice. A sequence names a coordinate
/// the read did stand on, so nothing is re-delivered; the cursor lags
/// instead, and where the tail is withheld it stops advancing at all.
///
/// The three grants are as far apart as this backend admits: one that
/// admits half the ledger, one that admits the other half, and one that
/// cannot be read and so admits none of it.
#[tokio::test]
async fn a_feed_position_denotes_the_same_ledger_prefix_under_every_grant() {
    let (plugin, ids) = feed_ledger().await;

    let pinned = feed_page(
        &plugin,
        &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
        FeedStart::Oldest,
        None,
        16,
    )
    .await;
    let other = feed_page(
        &plugin,
        &tenant_scope(FEED_OTHER_TENANT_ID),
        FeedStart::Oldest,
        None,
        16,
    )
    .await;
    let unreadable = feed_page(
        &plugin,
        &untranslatable_scope(),
        FeedStart::Oldest,
        None,
        16,
    )
    .await;

    // The three grants admit three different things. Without this the
    // position comparison below would be satisfied by a backend that
    // ignored the scope entirely, or by one that admitted nothing at all.
    assert_eq!(
        entry_ids(&pinned.entries),
        vec![ids[0], ids[2]],
        "the grant pinning the suite's tenant must carry that tenant's two entries, in the \
         ledger's append order, and neither of the other tenant's"
    );
    assert_eq!(
        entry_ids(&other.entries),
        vec![ids[1], ids[3]],
        "the grant pinning the other tenant must carry the complementary two entries: the scope \
         gates what the page carries"
    );
    assert!(
        unreadable.entries.is_empty(),
        "a grant this backend cannot translate admits nothing, the same fail-closed \
         disposition as the point lookup's, so its page carries no entry; it got: {:?}",
        entry_ids(&unreadable.entries)
    );

    // All three reads exhausted the ledger, so all three scanned it whole and
    // are handed its end. Under a bounded limit the positions differ and what
    // holds instead is that each resumes correctly.
    assert_eq!(
        pinned.next, other.next,
        "two callers whose grants admit disjoint halves of one ledger, both reading it to the \
         end, MUST be handed the same position: the position counts entries scanned, not \
         entries admitted. This equality is the exhausting case of the property that always \
         holds: a position denotes a ledger prefix, so it resumes correctly under any grant. \
         A position that moved with the grant could not be resumed by a caller whose grant had \
         since widened without silently skipping every entry the narrower grant withheld"
    );
    assert_eq!(
        other.next, unreadable.next,
        "a grant that cannot be read admits nothing and still scans everything, so it too MUST \
         be handed the position at the ledger's end. A cursor that stalled here would pin a \
         caller at the oldest position forever the moment one conjunct of its grant became \
         untranslatable"
    );
    assert_eq!(
        pinned.next,
        Some(feed_position(FEED_LEDGER_LEN)),
        "the position these reads reached is the ledger's end ({FEED_LEDGER_LEN}), which is \
         where scanning stopped rather than where any one grant's admissions did (2, 2 and \
         0). This literal is what pins a position's meaning: the two equalities above do fire \
         under a backend counting admitted entries, but they report only that the three \
         positions disagree and never which of them is right"
    );
}

/// A bounded `limit` hands two grants two positions, and each one resumes.
///
/// `limit` bounds the entries a page **admits** while the scan is unbounded,
/// so a page that stops on the limit stops where its own grant's entries run
/// out rather than where the ledger does. Over the A, B, A, B ledger at
/// `limit = 1` the grant pinning the suite's tenant is handed **1** and the
/// grant pinning the other tenant **2**: one scanned entry against two.
///
/// That is the read
/// [`a_feed_position_denotes_the_same_ledger_prefix_under_every_grant`]
/// cannot show, because its limit exhausts the ledger and every position it
/// compares is the ledger's end. Both tests assert one property and it is
/// resumability rather than identity: a position denotes a ledger prefix, the
/// same prefix under any grant, so resuming from it delivers every later
/// entry the resuming grant admits and skips none — asserted below in all
/// four combinations of the grant that minted a position and the grant that
/// resumes from it.
///
/// **This test no longer catches the admitted-count defect
/// [`a_feed_position_denotes_the_same_ledger_prefix_under_every_grant`]
/// describes, and it used to.** While a position was an offset, a cursor
/// counting admitted entries handed the other grant 1 rather than 2 and the
/// second literal below fired. A sequence is an entry's coordinate rather
/// than a count of entries, and the entry this grant admits *is* the ledger's
/// second, so the defect now satisfies both literals here and all four
/// resumptions. Measured, not derived: under that mutation this test passes
/// whole.
///
/// It keeps its place for what its name claims rather than for that
/// coverage — a bounded limit hands two grants two positions, which the
/// exhausting read in
/// [`a_feed_position_denotes_the_same_ledger_prefix_under_every_grant`]
/// cannot show — and for the four resumption assertions, which no other test
/// makes.
#[tokio::test]
async fn a_bounded_page_limit_hands_two_grants_two_positions_that_each_resume() {
    let (plugin, ids) = feed_ledger().await;
    let pinned_scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);
    let other_scope = tenant_scope(FEED_OTHER_TENANT_ID);

    let pinned = feed_page(&plugin, &pinned_scope, FeedStart::Oldest, None, 1).await;
    let other = feed_page(&plugin, &other_scope, FeedStart::Oldest, None, 1).await;

    assert_eq!(
        entry_ids(&pinned.entries),
        vec![ids[0]],
        "a limit of one admits one entry, and the first entry this grant admits is the ledger's \
         first"
    );
    assert_eq!(
        entry_ids(&other.entries),
        vec![ids[1]],
        "the first entry the other grant admits is the ledger's second, which is why its page \
         costs one more scanned entry than the page above"
    );
    assert_eq!(
        pinned.next,
        Some(feed_position(1)),
        "one entry scanned to admit one entry"
    );
    assert_eq!(
        other.next,
        Some(feed_position(2)),
        "two entries scanned to admit one entry: the withheld first entry still advances the \
         cursor past itself"
    );
    assert_ne!(
        pinned.next, other.next,
        "a bounded limit is exactly where two grants are handed two positions, so a test \
         asserting that two grants always agree on a position would be asserting something \
         this backend does not provide"
    );

    let from_pinned = pinned
        .next
        .expect("a live read carries a continuation on every page");
    let from_other = other
        .next
        .expect("a live read carries a continuation on every page");

    assert_eq!(
        feed_resume(&plugin, &pinned_scope, &from_pinned).await,
        vec![ids[2]],
        "the minting grant resumes after its own position and is handed the rest of what it \
         admits, once each"
    );
    assert_eq!(
        feed_resume(&plugin, &other_scope, &from_pinned).await,
        vec![ids[1], ids[3]],
        "the other grant resumes from a position it did not mint and is handed every entry it \
         admits after that prefix. Nothing it admits is skipped, which is the whole of what a \
         position promises across grants"
    );
    assert_eq!(
        feed_resume(&plugin, &pinned_scope, &from_other).await,
        vec![ids[2]],
        "resuming from the wider position skips nothing either: the entries it passed over are \
         the ledger's first two, and this grant's first entry is among them rather than beyond \
         them"
    );
    assert_eq!(
        feed_resume(&plugin, &other_scope, &from_other).await,
        vec![ids[3]],
        "and the grant that minted the wider position is handed the rest of what it admits"
    );
}

/// Resumes after `position` under `scope`, over a limit that exhausts the
/// ledger, and reports the entry ids the page carried.
///
/// The limit is the exhausting one deliberately: what a resumption assertion
/// is about is the whole of what follows a position, so a page bounded short
/// of it would report the limit rather than the position's meaning.
async fn feed_resume(
    plugin: &InMemoryReferencePlugin,
    scope: &ast::Expr,
    position: &FeedPosition,
) -> Vec<Uuid> {
    let page = feed_page(plugin, scope, FeedStart::After(position.clone()), None, 16).await;
    entry_ids(&page.entries)
}

/// A `limit`-bounded walk ends, and delivers each admitted entry once.
///
/// `limit` bounds the entries a page *carries*, so over a ledger whose
/// admitted and withheld entries alternate a page's cursor advances further
/// than the page is long. The walk below follows the cursor from `Oldest`
/// until it stops moving, which is the fixpoint at the ledger's end: an
/// empty page whose continuation is the position it was read from.
///
/// Delivering each entry exactly once is the whole point of the cursor
/// counting scanned entries. A backend that counted admitted ones would
/// re-scan every withheld entry on the following page — harmless here,
/// because a withheld entry is withheld again, and not harmless at all for
/// a caller whose grant widens between two pages.
#[tokio::test]
async fn a_limit_bounded_feed_walk_reaches_a_fixpoint_at_the_ledger_end() {
    let (plugin, ids) = feed_ledger().await;
    let scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);

    let mut start = FeedStart::Oldest;
    let mut delivered: Vec<Uuid> = Vec::new();
    let mut previous: Option<FeedPosition> = None;
    let mut reads = 0_u32;
    let fixpoint = loop {
        reads += 1;
        assert!(
            reads <= 16,
            "a walk with a page limit of one over a ledger of {FEED_LEDGER_LEN} entries must \
             reach its fixpoint in a handful of reads; {reads} says the cursor is not advancing \
             and a real gateway would be spinning here"
        );

        let FeedPage { entries, next } = feed_page(&plugin, &scope, start, None, 1).await;
        let next = next.expect(
            "a live read carries a continuation on every page, short and empty pages included: \
             an absent cursor is reserved for a bounded replay reaching its `until`",
        );
        if previous.as_ref() == Some(&next) {
            assert!(
                entries.is_empty(),
                "a page that did not move the cursor cannot have delivered anything: it would \
                 be delivering entries it is about to deliver again"
            );
            break next;
        }
        delivered.extend(entry_ids(&entries));
        previous = Some(next.clone());
        start = FeedStart::After(next);
    };

    assert_eq!(
        delivered,
        vec![ids[0], ids[2]],
        "the walk must deliver every admitted entry exactly once, in the ledger's append order. \
         A repeat means a page re-scanned what the previous page had already counted; a gap \
         means a page's cursor moved past an entry the page never carried"
    );
    assert_eq!(
        fixpoint,
        feed_position(FEED_LEDGER_LEN),
        "the fixpoint is the ledger's end, which is the number of entries scanned \
         ({FEED_LEDGER_LEN}) rather than the number delivered (2)"
    );
}

/// A bounded replay closes at the ledger's end, and only there.
///
/// An absent `next` is the one thing that says a bounded replay has reached
/// its `until` ([`crate::feed::FeedPage::next`]), so it has to be absent
/// exactly when the replay has. Bounded *past* the ledger's end the replay
/// has not reached anything yet: it keeps its cursor, and the caller resumes
/// there once the ledger has grown into the range it asked for.
#[tokio::test]
async fn a_bounded_feed_replay_closes_at_its_until_and_not_before() {
    let (plugin, ids) = feed_ledger().await;
    let scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);

    let at_the_end = feed_page(
        &plugin,
        &scope,
        FeedStart::Oldest,
        Some(feed_position(FEED_LEDGER_LEN)),
        16,
    )
    .await;
    assert_eq!(
        entry_ids(&at_the_end.entries),
        vec![ids[0], ids[2]],
        "a replay bounded at the ledger's end still carries everything the grant admits: the \
         bound is on the position, not on the page"
    );
    assert!(
        at_the_end.next.is_none(),
        "a replay whose cursor has reached its `until` is finished, and an absent `next` is \
         what says so. A position here is a page whose continuation the caller keeps \
         following, which is a hang rather than a wrong value. Got: {:?}",
        at_the_end.next
    );

    let past_the_end = feed_page(
        &plugin,
        &scope,
        FeedStart::Oldest,
        Some(feed_position(FEED_LEDGER_LEN + 5)),
        16,
    )
    .await;
    assert_eq!(
        past_the_end.next,
        Some(feed_position(FEED_LEDGER_LEN)),
        "a replay bounded past the ledger's end has not reached its `until`, so it keeps the \
         position it actually scanned to. Answering an absent cursor here would tell the caller \
         a range it never read was complete"
    );
}

/// A subscription no ledger entry answers to withholds every entry and still
/// scans the whole ledger.
///
/// The subscription is the other half of `read_feed_page`'s admission
/// decision, and the split it is on is the same one: it gates what the page
/// **carries**, never what the position **counts**. Two consumers reading
/// one backend under subscriptions of different breadth are handed
/// comparable positions for the same reason two grants are.
#[tokio::test]
async fn a_subscription_the_ledger_does_not_answer_still_advances_the_cursor() {
    let (plugin, _ids) = feed_ledger().await;
    let unsubscribed = MeterTypeId::new("gts.cf.core.uc.usage_record.v1~cf.core.uc.not_here.v1~")
        .expect("the fixture meter id is well formed");

    let page = plugin
        .read_feed_page(
            &[unsubscribed],
            &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
            FeedStart::Oldest,
            None,
            16,
        )
        .await
        .expect("a subscription naming an absent meter is a well-formed read, not a failure");

    assert!(
        page.entries.is_empty(),
        "every ledger entry carries the suite's meter, so a subscription naming another one \
         admits none of them; it got: {:?}",
        entry_ids(&page.entries)
    );
    assert_eq!(
        page.next,
        Some(feed_position(FEED_LEDGER_LEN)),
        "the position counts the {FEED_LEDGER_LEN} entries scanned even though the subscription \
         admitted none of them, exactly as it does for a grant that admits none of them"
    );
}

/// A position this backend did not issue is refused, on both paths that
/// decode one.
///
/// A plugin owns its own position encoding, so a foreign one is a
/// host-contract breach rather than a caller fault: `Internal`, never a
/// well-formed page read from a position that was guessed at. Both `start`
/// and `until` decode, so both refuse.
#[tokio::test]
async fn a_foreign_feed_position_is_refused_as_internal() {
    let (plugin, _ids) = feed_ledger().await;
    let scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);
    let foreign = FeedPosition::new(vec![1, 2, 3])
        .expect("three bytes is an admissible position, just not one this backend issues");

    let as_a_start = plugin
        .read_feed_page(
            &feed_subscription(),
            &scope,
            FeedStart::After(foreign.clone()),
            None,
            16,
        )
        .await
        .expect_err(
            "a position of the wrong width cannot be decoded, and guessing at it would resume a \
             feed from a point nobody named",
        );
    assert!(
        matches!(
            as_a_start,
            UsageCollectorPluginError::Internal(ref detail) if detail.contains("3 bytes")
        ),
        "a foreign `start` position MUST refuse as `Internal`, and the detail must name the \
         width it was handed so an operator can see which caller minted it; got: {as_a_start:?}"
    );

    let as_an_until = plugin
        .read_feed_page(
            &feed_subscription(),
            &scope,
            FeedStart::Oldest,
            Some(foreign),
            16,
        )
        .await
        .expect_err("the `until` bound decodes through the same encoding and refuses the same way");
    assert!(
        matches!(as_an_until, UsageCollectorPluginError::Internal(_)),
        "a foreign `until` position MUST refuse as `Internal` rather than be treated as an \
         unbounded replay, which would turn a bounded read into one that never closes; got: \
         {as_an_until:?}"
    );
}

/// A zero page limit is refused.
///
/// REST enforces `minimum: 1`, so a zero limit reaching the SPI is a
/// host-contract breach. It is refused rather than answered with an empty
/// page because a zero-limit page carries nothing and so advances the cursor
/// past nothing: `next` is the position it was read from, and a caller
/// following it under an `until` never reaches it.
#[tokio::test]
async fn a_zero_limit_feed_read_is_refused_as_internal() {
    let (plugin, _ids) = feed_ledger().await;

    let refused = plugin
        .read_feed_page(
            &feed_subscription(),
            &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
            FeedStart::Oldest,
            None,
            0,
        )
        .await
        .expect_err("the published page limit is at least one, so a zero limit is malformed");

    assert!(
        matches!(refused, UsageCollectorPluginError::Internal(_)),
        "a zero limit MUST refuse as a non-retryable host-contract breach rather than be served \
         as a well-formed page that cannot advance its own cursor; got: {refused:?}"
    );
}

// ---------------------------------------------------------------------------
//
// The reference backend's retention, and the feed refusal it drives. Still
// unit tests of the reference implementation rather than contract checks:
// DESIGN section 3.3's `feed-retention-refusal` is written against the SPI
// for any backend, is in `UNWRITTEN_CHECKS`, and lands with the entry point
// that hands `run_all` a driver. What the tests below establish is that the
// capability the check will be built on works, and works for the reasons
// DESIGN gives rather than by coincidence.

/// A covered-period floor `hours` past `FIXTURE_EPOCH`.
///
/// [`feed_ledger`]'s four entries end one, two, three and four hours past
/// that epoch, so a floor named in the same units reads as a count of the
/// entries it is above.
fn feed_floor(hours: i64) -> time::OffsetDateTime {
    super::fixtures::FIXTURE_EPOCH.saturating_add(time::Duration::hours(hours))
}

/// Drives the reference backend's retention over [`feed_ledger`]'s meter, to
/// a floor `hours` past `FIXTURE_EPOCH`.
async fn drop_feed_entries_before(plugin: &InMemoryReferencePlugin, hours: i64) {
    plugin
        .drop_before(&feed_meter(), feed_floor(hours))
        .await
        .expect("the reference backend drives its own retention without failing");
}

/// The meter of the entry a drop below must leave alone.
///
/// A second meter, written by no other test here and by no check: the drop
/// keys on a GTS type, and an entry on another type is how that half of the
/// key is observable at all.
const RETENTION_BYSTANDER_METER_ID: &str =
    "gts.cf.core.uc.usage_record.v1~cf.core.uc.retention_bystander.v1~";

/// Writes one entry on [`RETENTION_BYSTANDER_METER_ID`], covering the
/// earliest period any test here uses.
///
/// Its period ends an hour past `FIXTURE_EPOCH`, which is below every floor
/// driven below, so it survives a drop only because the drop named another
/// type. Returns the meter and the entry's id.
async fn write_the_retention_bystander(plugin: &InMemoryReferencePlugin) -> (MeterTypeId, Uuid) {
    let meter = MeterTypeId::new(RETENTION_BYSTANDER_METER_ID)
        .expect("the bystander meter id derives one segment from the published base type");
    let key = IdempotencyKey::new("retention-bystander").expect("the fixture key is well formed");
    let record = super::fixtures::fixture_record_on(
        meter.clone(),
        super::fixtures::CONTRACT_TENANT_ID,
        &key,
        UsageQuantity::parse("1").expect("fixture quantity"),
        super::fixtures::CONTRACT_ACCEPTED_AT,
        super::fixtures::FIXTURE_EPOCH,
        feed_floor(1),
    )
    .expect("the bystander fixture is projectable");
    let id = record.id;
    plugin
        .create_usage_record(record)
        .await
        .expect("the reference backend admits a well-formed entry");
    (meter, id)
}

/// Asserts one entry is still stored, under a grant that admits it.
///
/// The point lookup rather than a feed read, deliberately: what these tests
/// need to establish is whether an entry was **removed**, and the feed's own
/// answer is entangled with the refusal under test. `get_usage_record`
/// carries no retention refusal at all, so it reports storage and nothing
/// else.
async fn assert_still_stored(
    plugin: &InMemoryReferencePlugin,
    id: Uuid,
    scope: &ast::Expr,
    why: &str,
) {
    let found = plugin
        .get_usage_record(id, scope, false)
        .await
        .unwrap_or_else(|err| panic!("{why}; the lookup failed instead: {err:?}"));
    assert_eq!(found.id, id, "{why}");
}

/// A drop removes the entries of the type it names that end before its
/// floor, and no others.
///
/// Both halves of the key are load-bearing and both are read here. The
/// **floor** is a covered-period end, which is how DESIGN §3.1's
/// "Idempotency horizon" row measures retention — *"measured from the
/// entry's `window_end`"* — so a floor three hours past the fixture epoch
/// takes the two entries ending one and two hours past it and leaves the
/// two ending three and four. The **type** is why the drop can be driven
/// inside a shared ledger at all: `run_all` dispatches every check against
/// one backend that removes nothing of its own accord, and a drop keyed on
/// an instant alone would take every other check's fixtures with it. The
/// bystander entry ends an hour past the epoch, well below the floor, and
/// survives because it is metered on another type.
///
/// Every assertion is a point lookup rather than a feed read. Removal is
/// what this test is about; the refusal the removal enables is the next
/// three tests', and reading it here through the feed would leave a failure
/// unable to say which of the two broke.
#[tokio::test]
async fn a_retention_drop_removes_what_it_names_and_nothing_else() {
    let (plugin, ids) = feed_ledger().await;
    let (bystander_meter, bystander_id) = write_the_retention_bystander(&plugin).await;
    let pinned_scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);
    let other_scope = tenant_scope(FEED_OTHER_TENANT_ID);

    drop_feed_entries_before(&plugin, 3).await;

    assert_not_found(
        &plugin,
        ids[0],
        &pinned_scope,
        "the ledger's first entry ends one hour past the fixture epoch, below a floor three \
         hours past it, so the drop MUST have removed it. This lookup dispatches the grant that \
         admits that entry, so a `NotFound` here is removal rather than a scope gate",
    )
    .await;
    assert_not_found(
        &plugin,
        ids[1],
        &other_scope,
        "the ledger's second entry ends two hours past the fixture epoch and belongs to the \
         other tenant. The drop keys on the covered period and the type, never on the tenant, \
         so it MUST have removed this one too",
    )
    .await;
    assert_still_stored(
        &plugin,
        ids[2],
        &pinned_scope,
        "the ledger's third entry ends exactly at the floor, and the bound is exclusive: an \
         entry whose period ends at the floor is inside the retention the floor expresses. A \
         drop that took it would be removing an entry the floor still covers",
    )
    .await;
    assert_still_stored(
        &plugin,
        ids[3],
        &other_scope,
        "the ledger's fourth entry ends past the floor and MUST survive: a drop removes what \
         its floor names and stops there",
    )
    .await;
    assert_still_stored(
        &plugin,
        bystander_id,
        &pinned_scope,
        "the bystander entry ends an hour past the fixture epoch, further below the floor than \
         either removed entry, and is metered on another type. It MUST survive: a drop keyed on \
         an instant alone would take one check's fixtures out of another check's ledger, which \
         is exactly what `run_all` dispatching every check against one backend cannot afford",
    )
    .await;

    let bystander_page = plugin
        .read_feed_page(
            &[bystander_meter],
            &pinned_scope,
            FeedStart::Oldest,
            None,
            16,
        )
        .await
        .expect("a subscription to the untouched meter is a well-formed read");
    assert_eq!(
        entry_ids(&bystander_page.entries),
        vec![bystander_id],
        "and the untouched type's own subscription still serves it, so the drop left the entry \
         readable through the feed and not only through the point lookup"
    );
}

/// A cursor after which a drop removed an entry is refused.
///
/// DESIGN §3.3's `feed-retention-refusal`: *"A cursor after which retention
/// has removed an entry of a subscribed GTS type is refused rather than
/// served as a short page"*. The floor four hours past the fixture epoch
/// removes the ledger's first three entries and raises the meter's mark to
/// the third of them, so the cursor below, which sits at the second, has
/// lost the entry that followed it.
///
/// **The lost entry is one this grant admits, deliberately.** That is what
/// separates the plain refusal from the scope-independence
/// [`the_retention_refusal_does_not_consult_the_callers_scope`] measures,
/// which reads the same drop from the same cursor under the other grant. A
/// backend deciding the refusal from what the grant would have delivered
/// still refuses here, and a backend that refused nothing at all does not.
///
/// The alternative this refuses is worse than an error and quieter: an
/// empty page, indistinguishable from a feed with nothing new on it, in
/// place of the entry this consumer was about to bill for.
#[tokio::test]
async fn a_cursor_a_retention_drop_passed_is_refused() {
    let (plugin, _ids) = feed_ledger().await;
    drop_feed_entries_before(&plugin, 4).await;

    let refused = plugin
        .read_feed_page(
            &feed_subscription(),
            &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
            FeedStart::After(feed_position(2)),
            None,
            16,
        )
        .await
        .expect_err(
            "a cursor at the ledger's second entry, after which the drop removed the third, has \
             lost its continuation",
        );

    assert!(
        matches!(refused, UsageCollectorPluginError::CursorBeyondRetention),
        "the refusal MUST be `CursorBeyondRetention`, which is the caller-actionable one DESIGN \
         §3.3 names: a consumer told its cursor is past retention rebootstraps, and one handed \
         a short page silently under-bills. Got: {refused:?}"
    );
}

/// `FeedStart::Oldest` is never refused on the retention floor.
///
/// DESIGN §3.3's `feed-bootstrap-position`: it *"begins at the oldest entry
/// the subscription retains, never at the head, and is never refused on the
/// retention floor"*. It cannot have lost a continuation, because it asks
/// for no particular one: it asks for whatever is still retained.
///
/// The marks are therefore not consulted at all here, rather than consulted
/// and found not to fire. The drop below raises a mark that would refuse a
/// cursor at the ledger's first entry — the previous test is that read — and
/// this one is served whole.
#[tokio::test]
async fn the_oldest_start_is_never_refused_on_the_retention_floor() {
    let (plugin, ids) = feed_ledger().await;
    drop_feed_entries_before(&plugin, 3).await;

    let page = feed_page(
        &plugin,
        &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
        FeedStart::Oldest,
        None,
        16,
    )
    .await;

    assert_eq!(
        entry_ids(&page.entries),
        vec![ids[2]],
        "a bootstrap after a drop begins at the oldest entry still retained, which is the \
         ledger's third, and this grant admits it"
    );
    assert_eq!(
        page.next,
        Some(feed_position(FEED_LEDGER_LEN)),
        "and its cursor still counts the entries it scanned, which are the two the drop left"
    );
}

/// A cursor whose continuation is intact is served, at the mark itself.
///
/// DESIGN §3.3: *"A cursor whose continuation is intact is served"*. The
/// comparison against the mark is therefore strict. A cursor sitting exactly
/// at the highest sequence a drop removed has lost nothing **after** itself:
/// every entry the drop took is at or before it, and it was never going to
/// be delivered them again.
///
/// The floor three hours past the fixture epoch raises the mark to 2, and
/// the read below resumes from 2. Comparing `>=` instead of `>` refuses it,
/// which is a consumer rebootstrapping a whole feed because retention caught
/// up with the last page it had already processed.
#[tokio::test]
async fn a_cursor_at_the_retention_mark_is_still_served() {
    let (plugin, ids) = feed_ledger().await;
    drop_feed_entries_before(&plugin, 3).await;

    let page = feed_page(
        &plugin,
        &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
        FeedStart::After(feed_position(2)),
        None,
        16,
    )
    .await;

    assert_eq!(
        entry_ids(&page.entries),
        vec![ids[2]],
        "the continuation after the mark is intact, so it is served: the ledger's third entry \
         is the one this grant admits there"
    );
    assert_eq!(
        page.next,
        Some(feed_position(FEED_LEDGER_LEN)),
        "and the page reaches the ledger's end, the two entries the drop left having been \
         scanned"
    );
}

/// A drive that removes nothing refuses nothing, however old the cursor.
///
/// The mark records what a sweep **actually removed**, never what a floor
/// would have permitted it to remove, which is what DESIGN §3.3 means by *"A
/// cursor whose continuation is intact is served, including one older than
/// the floor where the plugin retains longer than it"*. The floor driven
/// here is one hour past the fixture epoch and the ledger's earliest entry
/// ends exactly there, so the exclusive bound leaves it and no mark is
/// raised at all — and the cursor below, which sits on an entry at that same
/// floor, is served rather than refused.
///
/// What this pins is that a mark is raised by a removal rather than by a
/// drive. A drive that took nothing leaves the marks empty, and an empty
/// mark refuses nothing, however old the cursor compared against it.
#[tokio::test]
async fn a_floor_that_removed_nothing_refuses_no_cursor() {
    let (plugin, ids) = feed_ledger().await;
    drop_feed_entries_before(&plugin, 1).await;

    assert_still_stored(
        &plugin,
        ids[0],
        &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
        "the ledger's earliest entry ends exactly at this floor, and the bound is exclusive, so \
         the drive removed nothing",
    )
    .await;

    let page = feed_page(
        &plugin,
        &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
        FeedStart::After(feed_position(1)),
        None,
        16,
    )
    .await;
    assert_eq!(
        entry_ids(&page.entries),
        vec![ids[2]],
        "a drive that removed nothing raises no mark, so a cursor at the floor's own entry is \
         served the rest of what this grant admits"
    );
}

/// The refusal does not consult the caller's scope.
///
/// DESIGN §3.3 refuses *"whether or not the caller's scope admitted that
/// entry"*, and `usage-collector-v1.yaml` states the mechanism it follows
/// from: removal *"is read per subscribed GTS type, so a cursor can be
/// refused for an entry the caller's own scope excluded"*.
///
/// The read below is exactly that case. The grant pins the other tenant, and
/// it mints its own cursor: over the ledger's alternating tenants a page
/// limited to one entry admits the ledger's second and scans two, so the
/// cursor is 2. The floor four hours past the fixture epoch then removes the
/// ledger's first three entries, and **the only one of them after that
/// cursor is the third, which belongs to the tenant this grant never
/// admitted**. Everything this grant can still read after its cursor — the
/// ledger's fourth entry — is intact, asserted below through the point
/// lookup so that the feed's own refusal is not the thing reporting it.
///
/// So a backend deciding the refusal from what the grant would have
/// delivered serves this page, and a backend reading the mark refuses it.
/// That is the whole distance between the two implementations, and this is
/// the read that measures it.
#[tokio::test]
async fn the_retention_refusal_does_not_consult_the_callers_scope() {
    let (plugin, ids) = feed_ledger().await;
    let other_scope = tenant_scope(FEED_OTHER_TENANT_ID);

    let minted = feed_page(&plugin, &other_scope, FeedStart::Oldest, None, 1).await;
    assert_eq!(
        entry_ids(&minted.entries),
        vec![ids[1]],
        "the first entry this grant admits is the ledger's second"
    );
    let cursor = minted
        .next
        .expect("a live read carries a continuation on every page");
    assert_eq!(
        cursor,
        feed_position(2),
        "two entries scanned to admit one, so the cursor this grant minted for itself sits at \
         the ledger's second entry"
    );

    drop_feed_entries_before(&plugin, 4).await;

    assert_still_stored(
        &plugin,
        ids[3],
        &other_scope,
        "the ledger's fourth entry is the only one after this cursor that this grant admits, \
         and the drop left it. Without this the refusal below would be satisfied by a backend \
         that had removed the grant's own continuation",
    )
    .await;

    let refused = plugin
        .read_feed_page(
            &feed_subscription(),
            &other_scope,
            FeedStart::After(cursor),
            None,
            16,
        )
        .await
        .expect_err(
            "retention removed the ledger's third entry, which is after this cursor and of a \
             subscribed type, so the cursor is refused even though this grant would never have \
             been handed that entry",
        );

    assert!(
        matches!(refused, UsageCollectorPluginError::CursorBeyondRetention),
        "the refusal MUST be decided from what the sweep removed, not from what this grant \
         would have delivered. A backend that intersected the removed entries with the scope \
         would serve this page and be right about this caller's entries and wrong about the \
         contract: DESIGN refuses whatever the scope admitted, and the wire refusal is \
         published on the same terms. Got: {refused:?}"
    );
}

// ---------------------------------------------------------------------------
//
// The reference backend's fold and its reconciliation figures. Unit tests of
// the reference implementation rather than contract checks, and the halves
// have parted: DESIGN section 3.3's `latest-tie-break` has landed, so the
// `LATEST` order below is now asserted against every plugin as well as here,
// while no check of the sixteen reads a reconciliation quantity summary at
// all. What the tests below hold is the suite's own subject to those rules,
// which is how the check was validated before it was pointed at anything
// else.

/// The meter the tests below write to.
///
/// The suite's shared one, which `run_all`'s constraint on sharing does not
/// reach: every test here drives a backend of its own, so no other check's
/// entries are in the ledger it reads.
fn fold_meter() -> MeterTypeId {
    MeterTypeId::new(super::fixtures::CONTRACT_METER_TYPE_ID)
        .expect("the suite's own meter type id is valid")
}

/// The single bucket a no-grouping fold over `range` reports.
///
/// `group_by` is empty, which the aggregate surface fixes as the no-grouping
/// case: one bucket carrying an empty key. Any other bucket count is a
/// failure here rather than something to pick a value out of.
async fn ungrouped_fold(
    plugin: &InMemoryReferencePlugin,
    range: TimeRange,
    fold: AggregationFold,
) -> Option<BigDecimal> {
    let result = plugin
        .query_aggregated_usage_records(
            fold_meter(),
            range,
            fold,
            &super::fixtures::contract_query(16),
            &[],
            &[],
        )
        .await
        .unwrap_or_else(|err| {
            panic!("the reference backend folds {fold:?} without failing: {err:?}")
        });
    let count = result.buckets.len();
    let mut buckets = result.buckets.into_iter();
    match (buckets.next(), buckets.next()) {
        (Some(bucket), None) => {
            assert!(
                bucket.key.is_empty(),
                "the no-grouping case is one bucket carrying an empty key; {fold:?} reported \
                 the key {:?}",
                bucket.key
            );
            bucket.value
        }
        _ => {
            panic!("the no-grouping case is exactly one bucket; {fold:?} reported {count} of them")
        }
    }
}

/// The covered period both entries of the `LATEST` tie share.
const LATEST_TIE_WINDOW_START: time::OffsetDateTime =
    super::fixtures::FIXTURE_EPOCH.saturating_add(time::Duration::days(180));
/// The end of that period, and the first of DESIGN section 3.1's three keys.
/// Shared, so the tie-break has to reach the second.
const LATEST_TIE_WINDOW_END: time::OffsetDateTime =
    LATEST_TIE_WINDOW_START.saturating_add(time::Duration::hours(1));

/// The acceptance instant the later-accepted entry carries: an hour past
/// [`CONTRACT_ACCEPTED_AT`](super::fixtures::CONTRACT_ACCEPTED_AT), which the
/// earlier one keeps.
const LATEST_TIE_LATER_ACCEPTED_AT: time::OffsetDateTime =
    super::fixtures::CONTRACT_ACCEPTED_AT.saturating_add(time::Duration::hours(1));

/// The earlier-accepted entry's quantity, and what a fold reading
/// `(window_end, id)` alone reports.
const LATEST_TIE_EARLIER_QUANTITY: &str = "11";
/// The later-accepted entry's quantity, and what DESIGN section 3.1's order
/// reports. Distinct from the one above, which is the whole assertion.
const LATEST_TIE_LATER_QUANTITY: &str = "22";

/// One entry of the tie, on the shared meter and tenant.
fn latest_tie_break_entry(
    role: &str,
    attempt: u32,
    quantity: &str,
    accepted_at: time::OffsetDateTime,
) -> UsageRecord {
    let key = IdempotencyKey::new(format!("latest-tie-break-{role}-{attempt}"))
        .expect("the fixture key is well formed");
    super::fixtures::fixture_record_on(
        fold_meter(),
        super::fixtures::CONTRACT_TENANT_ID,
        &key,
        UsageQuantity::parse(quantity).expect("fixture quantity"),
        accepted_at,
        LATEST_TIE_WINDOW_START,
        LATEST_TIE_WINDOW_END,
    )
    .expect("the tie-break fixture is projectable")
}

/// Two entries sharing a covered period, differing in `accepted_at`, where
/// the later-accepted one carries the **smaller** `id`.
///
/// **The inversion is what makes the test able to fail.** A fold keyed on
/// `(window_end, id)` and one keyed on `(window_end, accepted_at, id)` pick
/// the same winner unless the two keys disagree, so a fixture whose
/// later-accepted entry also carries the greater `id` would pass under either
/// and prove nothing.
///
/// It cannot simply be chosen. An `id` is the `UUIDv5` over the six identity
/// inputs, so which of two entries carries the greater one is a property of
/// the derived values; the search varies the one input that is free here, the
/// idempotency key, and stops at the first index that inverts. Returns the
/// two entries and the index they were found at.
///
/// The search itself is [`super::fixtures::inverted_id_pair`] rather than a
/// loop of this test's own, and it is shared with the
/// [`latest_tie_break`](super::checks::latest_tie_break()) contract check,
/// which needs the same inversion twice. What is shared is the search and
/// the bound; what stays here is why *this* pair needs it, which the helper
/// takes as its `purpose` and reports if it never inverts.
fn latest_tie_break_pair() -> (UsageRecord, UsageRecord, u32) {
    let (later, earlier, attempt) = super::fixtures::inverted_id_pair(
        "The pair this test needs is one whose later-accepted entry carries the smaller `id`: \
         without the inversion a fold keyed on `(window_end, id)` alone picks the same winner \
         as one keyed on `(window_end, accepted_at, id)`, and this test would pass while \
         asserting nothing.",
        |attempt| {
            Ok((
                latest_tie_break_entry(
                    "later",
                    attempt,
                    LATEST_TIE_LATER_QUANTITY,
                    LATEST_TIE_LATER_ACCEPTED_AT,
                ),
                latest_tie_break_entry(
                    "earlier",
                    attempt,
                    LATEST_TIE_EARLIER_QUANTITY,
                    super::fixtures::CONTRACT_ACCEPTED_AT,
                ),
            ))
        },
    )
    .unwrap_or_else(|detail| panic!("{detail}"));
    (earlier, later, attempt)
}

/// `LATEST` breaks a `window_end` tie on the greater `accepted_at`, the
/// middle key of DESIGN section 3.1's three.
///
/// The declared order is *"Greatest `window_end`, then greatest
/// `accepted_at`, then greatest `id` in byte order"*, and the two entries
/// here share the first key and disagree on the second and the third in
/// opposite directions. So the fold reports the later-accepted entry's
/// quantity if it reads `accepted_at` and the earlier one's if it falls
/// straight through to `id`.
///
/// It is also the first caller to pass
/// [`fixture_record_on`](super::fixtures::fixture_record_on) an
/// `accepted_at` other than
/// [`CONTRACT_ACCEPTED_AT`](super::fixtures::CONTRACT_ACCEPTED_AT), so it is
/// what establishes that the parameter reaches the projection at all. A
/// builder that dropped it would stamp both entries with one instant, and
/// the second guard below - which is there for this and not as a restatement
/// of the fixture - reports that rather than letting the fold be asserted
/// over a tie the two entries do not actually have.
#[tokio::test]
async fn latest_breaks_a_window_end_tie_on_the_greater_accepted_at() {
    let plugin = InMemoryReferencePlugin::new();
    let (earlier, later, attempt) = latest_tie_break_pair();
    assert_eq!(
        earlier.window_end, later.window_end,
        "the two entries MUST share the first key for the tie-break to reach the second"
    );
    assert!(
        earlier.accepted_at < later.accepted_at,
        "the entry named `later` MUST carry the greater acceptance instant"
    );
    for record in [earlier.clone(), later.clone()] {
        plugin
            .create_usage_record(record)
            .await
            .expect("the reference backend admits a well-formed entry");
    }
    let range = TimeRange::new(
        LATEST_TIE_WINDOW_START,
        LATEST_TIE_WINDOW_END.saturating_add(time::Duration::hours(1)),
    )
    .expect("the tie-break read range is ordered");

    let latest = ungrouped_fold(&plugin, range, AggregationFold::Latest).await;

    assert_eq!(
        latest,
        Some(
            BigDecimal::from_str(LATEST_TIE_LATER_QUANTITY)
                .expect("the fixture quantity is a decimal")
        ),
        "DESIGN section 3.1 declares the `LATEST` order as greatest `window_end`, then greatest \
         `accepted_at`, then greatest `id` in byte order. These two entries share a \
         `window_end`, and the one accepted an hour later carries the smaller `id` (the pair \
         found at attempt {attempt}: later `{later_id}` below earlier `{earlier_id}`), so a \
         fold that skipped the middle key would report the earlier entry's \
         `{LATEST_TIE_EARLIER_QUANTITY}` instead of the later entry's \
         `{LATEST_TIE_LATER_QUANTITY}`",
        later_id = later.id,
        earlier_id = earlier.id,
    );
}

/// The lower bound of the range the empty-selection test folds over.
const EMPTY_FOLD_FROM: time::OffsetDateTime =
    super::fixtures::FIXTURE_EPOCH.saturating_add(time::Duration::days(270));
/// Its exclusive upper bound.
const EMPTY_FOLD_TO: time::OffsetDateTime =
    EMPTY_FOLD_FROM.saturating_add(time::Duration::hours(1));

/// An empty selection answers `0` under `SUM` and `COUNT` and absent under
/// `MAX`, `MIN` and `LATEST`.
///
/// DESIGN section 3.3's plugin obligations: *"`SUM` and `COUNT` are defined
/// over an empty selection and report `0`; `MAX`, `MIN` and `LATEST` are not
/// and report absent"*.
///
/// **The split is the assertion**, and neither half carries it alone. A
/// backend that answers absent to every fold - which is also what a backend
/// computing no fold at all answers - satisfies the last three and fails the
/// first two; one that answers zero to every fold satisfies the first two
/// and fails the last three. Only a backend that divides them the way DESIGN
/// does passes both halves.
///
/// The ledger is not empty. One entry sits an hour below the range's lower
/// bound, so the emptiness is the range's doing rather than the backend's
/// having nothing to read - which is what a real plugin's `WHERE` clause
/// would be answering too.
#[tokio::test]
async fn an_empty_selection_splits_zero_from_absent_by_fold() {
    let plugin = InMemoryReferencePlugin::new();
    let key = IdempotencyKey::new("empty-fold-bystander").expect("the fixture key is well formed");
    let bystander = super::fixtures::fixture_record_on(
        fold_meter(),
        super::fixtures::CONTRACT_TENANT_ID,
        &key,
        UsageQuantity::parse("500").expect("fixture quantity"),
        super::fixtures::CONTRACT_ACCEPTED_AT,
        EMPTY_FOLD_FROM.saturating_sub(time::Duration::hours(2)),
        EMPTY_FOLD_FROM.saturating_sub(time::Duration::hours(1)),
    )
    .expect("the bystander fixture is projectable");
    plugin
        .create_usage_record(bystander)
        .await
        .expect("the reference backend admits a well-formed entry");
    let range = TimeRange::new(EMPTY_FOLD_FROM, EMPTY_FOLD_TO)
        .expect("the empty-selection read range is ordered");

    for (fold, defined) in [
        (AggregationFold::Sum, true),
        (AggregationFold::Count, true),
        (AggregationFold::Max, false),
        (AggregationFold::Min, false),
        (AggregationFold::Latest, false),
    ] {
        let value = ungrouped_fold(&plugin, range, fold).await;
        if defined {
            assert_eq!(
                value,
                Some(BigDecimal::from(0)),
                "DESIGN section 3.3 defines {fold:?} over an empty selection and has it report \
                 `0`. The range holds no entry - the one entry stored ends an hour below its \
                 lower bound - and the no-grouping bucket is still emitted, so the fold has an \
                 empty selection to answer over"
            );
        } else {
            assert!(
                value.is_none(),
                "DESIGN section 3.3 does not define {fold:?} over an empty selection and has it \
                 report absent: there is no row to read a quantity off, and zero is a quantity \
                 rather than the absence of one. Got {value:?}"
            );
        }
    }
}

/// The lower bound of the range the reconciliation test reports over.
const RECONCILIATION_FROM: time::OffsetDateTime =
    super::fixtures::FIXTURE_EPOCH.saturating_add(time::Duration::days(360));
/// Its exclusive upper bound, past the end of every entry written below.
const RECONCILIATION_TO: time::OffsetDateTime =
    RECONCILIATION_FROM.saturating_add(time::Duration::hours(3));

/// The surviving entry's quantity, and the whole of the summary under `SUM`.
const RECONCILIATION_LIVE_QUANTITY: &str = "7.25";
/// The withdrawn entry's quantity, echoed by the invalidation that withdraws
/// it. Distinct from the one above and large beside it, so a summary that
/// kept either half of the pair is a different number rather than a near one.
const RECONCILIATION_WITHDRAWN_QUANTITY: &str = "1000";

/// Reconciliation reports the declared fold over the range, with withdrawn
/// pairs left out, while the count keeps them.
///
/// The two figures answer different questions, and DESIGN section 3.3's
/// plugin obligations say so: `accepted_count` *"counts every accepted entry
/// the range selects, invalidations included, because it reports ingestion
/// activity rather than aggregating the meter; the quantity summary excludes
/// withdrawn pairs"*.
///
/// Three entries are written: one live, one withdrawn, and the invalidation
/// withdrawing it. So the count is three and the `SUM` summary is the live
/// entry's quantity alone - a summary that kept the withdrawn record adds
/// that record's quantity on top of it, and one that kept the echoing
/// invalidation too adds it twice.
///
/// The second dispatch is the same range under `COUNT`, which reports one.
/// It is what holds the summary to reading the `fold` parameter: one
/// hard-wired to `SUM` answers the live entry's quantity there, and one
/// hard-wired to absent answers nothing under either fold.
#[tokio::test]
async fn reconciliation_folds_the_range_and_leaves_out_withdrawn_pairs() {
    let plugin = InMemoryReferencePlugin::new();
    let live_key =
        IdempotencyKey::new("reconciliation-live").expect("the fixture key is well formed");
    let withdrawn_key =
        IdempotencyKey::new("reconciliation-withdrawn").expect("the fixture key is well formed");
    let live = super::fixtures::fixture_record_on(
        fold_meter(),
        super::fixtures::CONTRACT_TENANT_ID,
        &live_key,
        UsageQuantity::parse(RECONCILIATION_LIVE_QUANTITY).expect("fixture quantity"),
        super::fixtures::CONTRACT_ACCEPTED_AT,
        RECONCILIATION_FROM,
        RECONCILIATION_FROM.saturating_add(time::Duration::hours(1)),
    )
    .expect("the live fixture is projectable");
    let withdrawn = super::fixtures::fixture_record_on(
        fold_meter(),
        super::fixtures::CONTRACT_TENANT_ID,
        &withdrawn_key,
        UsageQuantity::parse(RECONCILIATION_WITHDRAWN_QUANTITY).expect("fixture quantity"),
        super::fixtures::CONTRACT_ACCEPTED_AT,
        RECONCILIATION_FROM.saturating_add(time::Duration::hours(1)),
        RECONCILIATION_FROM.saturating_add(time::Duration::hours(2)),
    )
    .expect("the withdrawn fixture is projectable");
    let invalidation = super::fixtures::fixture_invalidation(&withdrawn)
        .expect("the withdrawal fixture is projectable");
    for record in [live, withdrawn, invalidation] {
        plugin
            .create_usage_record(record)
            .await
            .expect("the reference backend admits a well-formed entry");
    }
    let range = TimeRange::new(RECONCILIATION_FROM, RECONCILIATION_TO)
        .expect("the reconciliation range is ordered");
    let scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);

    let summed = plugin
        .get_reconciliation_metadata(
            super::fixtures::CONTRACT_TENANT_ID,
            fold_meter(),
            range,
            AggregationFold::Sum,
            &scope,
        )
        .await
        .expect("the reference backend reports reconciliation metadata without failing");

    assert_eq!(
        summed.accepted_count, 3,
        "`accepted_count` reports ingestion activity, so it counts the invalidation and the \
         record it withdraws alongside the surviving entry"
    );
    assert_eq!(
        summed.quantity_summary,
        Some(
            BigDecimal::from_str(RECONCILIATION_LIVE_QUANTITY)
                .expect("the fixture quantity is a decimal")
        ),
        "the quantity summary is the declared fold over the range with withdrawn pairs left \
         out, so it is the live entry's `{RECONCILIATION_LIVE_QUANTITY}` alone. A summary \
         keeping only the record would report `{RECONCILIATION_WITHDRAWN_QUANTITY}` more, and \
         one keeping the echoing invalidation too would report twice that; an absent summary is \
         a backend that filled in the count and the watermarks and forgot the field"
    );

    let counted = plugin
        .get_reconciliation_metadata(
            super::fixtures::CONTRACT_TENANT_ID,
            fold_meter(),
            range,
            AggregationFold::Count,
            &scope,
        )
        .await
        .expect("the reference backend reports reconciliation metadata without failing");

    assert_eq!(
        counted.accepted_count, 3,
        "`accepted_count` does not read the fold: it is the same three entries"
    );
    assert_eq!(
        counted.quantity_summary,
        Some(BigDecimal::from(1)),
        "`COUNT` counts the records in range that no accepted invalidation withdraws, which is \
         the one surviving entry. This dispatch is what holds the summary to reading the `fold` \
         parameter rather than hard-wiring one"
    );
}
