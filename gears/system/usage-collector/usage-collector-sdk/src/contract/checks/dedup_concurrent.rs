//! The DESIGN §3.3 `dedup-concurrent` check.
//!
//! See [`dedup_concurrent`] for what it asserts, including why driving two
//! concurrent tasks at one backend handle is a faithful reduction of
//! DESIGN's "several gateway replicas" and which of the row's clauses this
//! check states without exercising. The module holds the two identities it
//! races, the divergent submission that collides with one of them, and the
//! three read surfaces the `Eventual` half compares against each other.

use bigdecimal::BigDecimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, check_meter, check_window_from, contract_query,
    contract_scope, fixture_record_on, violation,
};
use crate::contract::{ContractViolation, DEDUP_CONCURRENT, DedupLevel, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPosition, FeedStart};
use crate::models::{AggregationFold, IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The start of the covered period every entry of this check carries, and
/// the inclusive lower bound of the range its `Eventual` half reads them
/// back over.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives. It matters here because the `Eventual` half counts
/// the rows a range returns against the identities this check raced, so a
/// stray entry inside it would be read as a row no identity of this check
/// accounts for — which is the shape of the failure that half exists to
/// report.
///
/// The offset is the second of two separations rather than the only one:
/// this check also reads over a meter of its own (see
/// [`dedup_concurrent_fixtures`]), and a range and a meter that no other
/// check writes to are independent reasons why nothing else can land in the
/// page, the fold or the feed page this check counts.
const DEDUP_CONCURRENT_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(DEDUP_CONCURRENT, "main");

/// The end of that covered period.
///
/// **Every entry of this check carries one period**, and that is deliberate
/// for the reason `dedup-floor`'s own period end gives over its own: the two identities here are separated by their idempotency keys
/// alone, so no key of this check is ever submitted over a second period,
/// and `contract_mutants`'s `Defect::DedupIgnoresThePeriod` — an index on
/// the derived identity with the two period bounds struck out — answers
/// exactly as the full identity does for everything this check submits.
const DEDUP_CONCURRENT_WINDOW_END: time::OffsetDateTime =
    DEDUP_CONCURRENT_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The exclusive upper bound of the range the `Eventual` half reads over, an
/// hour past [`DEDUP_CONCURRENT_WINDOW_END`].
///
/// The range holds **both** covered-period bounds, deliberately, for the
/// reason `dedup-floor`'s own upper bound gives: this check asserts nothing
/// about period selection, and a range beginning at
/// [`DEDUP_CONCURRENT_WINDOW_END`] would make all three of its read surfaces
/// unanswerable for a backend selecting on `window_start`, coupling this
/// check to `window-end-selection`'s rule for no reason of its own.
const DEDUP_CONCURRENT_WINDOW_TO: time::OffsetDateTime =
    DEDUP_CONCURRENT_WINDOW_END.saturating_add(time::Duration::hours(1));

/// The quantity both racers of the identical pair carry, and the quantity
/// the first of the divergent pair carries.
///
/// Exactly representable in a binary float, which keeps the one subject that
/// rewrites a quantity on the way in — `contract_mutants`'s
/// `Defect::QuantityThroughFloat` — from changing anything this check can
/// see. Every assertion here compares what a plugin answered against a
/// fixture this module built, so a quantity that moved through that rewrite
/// would come back unequal to its fixture and be reported by this check for
/// a mistake that is `quantity-round-trip`'s rule rather than this one's.
const DEDUP_CONCURRENT_QUANTITY: &str = "5.5";

/// The quantity the **divergent** racer carries instead.
///
/// The divergence has to be in a caller-supplied field that is not one of
/// the six identity inputs, so that the submission derives its partner's own
/// `id` and still fails [`UsageRecord::caller_supplied_eq`]. The quantity is
/// such a field: DESIGN §3.1's "Dedup identity" gives the identity as
/// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end,
/// entry_type)` and "Identity derivation" derives `id` from *"all six
/// components in order"* and nothing else, while "Collision resolution"
/// compares *"exact equality of the caller-supplied fields"*, the quantity
/// among them.
///
/// Exactly representable in a binary float for the reason
/// [`DEDUP_CONCURRENT_QUANTITY`] gives, with a second reason of its own: a
/// rewrite that landed both quantities on one value would make the divergent
/// racer an identical one, and the assertion that the race yields a conflict
/// would then hold a conforming backend to an outcome it must not give. That
/// the two are distinct as the plugin is handed them is
/// [`DedupConcurrentFixtures::guards`]' business rather than this comment's.
const DEDUP_CONCURRENT_DIVERGENT_QUANTITY: &str = "7.25";

/// The read limit the `Eventual` half dispatches, on the ledger page, in the
/// fold's query and on each feed page alike: twice the two identities it
/// expects.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives, and it carries the weight it carries in `dedup-floor`: **the row
/// count is the whole of the `Eventual` half**. A limit set to the expected
/// two would truncate a third row away, and the assertion that only one
/// write of a raced identity ever shows would pass against a backend holding
/// both.
const DEDUP_CONCURRENT_PAGE_LIMIT: u64 = 4;

/// How many feed pages the `Eventual` half will follow before it gives up.
///
/// The feed is **followed** rather than read once, for the reason
/// `server-field-round-trip`'s own page budget gives: a short page is conforming, so a single read would report
/// a backend that pages in small steps as having lost entries. The loop's
/// real exit is the cursor standing still; this budget bounds the loop
/// rather than the read.
const DEDUP_CONCURRENT_FEED_PAGES: usize = 256;

/// The two identities this check races, and the meter and range its
/// `Eventual` half reads them back over.
struct DedupConcurrentFixtures {
    /// The meter every entry is written to, the only meter this check reads,
    /// and the whole of the subscription its feed read dispatches.
    meter: MeterTypeId,
    /// The range all three `Eventual` surfaces dispatch, built once.
    ///
    /// Built here rather than at each read so that a range the suite cannot
    /// construct is reported as [`HARNESS_FAULT`] along with every other
    /// fixture fault, rather than three times against the plugin under three
    /// different messages about a failure that is not the plugin's.
    range: TimeRange,
    /// One racer of the identical pair.
    identical: UsageRecord,
    /// The other racer of the identical pair.
    ///
    /// Built by a second call to the same builder rather than cloned from
    /// [`Self::identical`], so that "the two are identical" is a fact
    /// [`DedupConcurrentFixtures::guards`] establishes about the builder
    /// rather than a fact about the language. A clone would make the guard
    /// assert that a value equals itself.
    identical_again: UsageRecord,
    /// One racer of the divergent pair.
    divergent: UsageRecord,
    /// The other racer of the divergent pair: the same six identity inputs
    /// as [`Self::divergent`], a different quantity.
    divergent_other: UsageRecord,
}

/// One race's result: the identity it left on the ledger, and what it
/// reported.
///
/// The two are separate for the reason `dedup-floor`'s `Scenario` keeps them
/// separate: a race can seed its identity **and** report a violation, and
/// the `Eventual` half below then counts rows for the identities that were
/// actually seeded, so a refused race is reported once, by the race that met
/// it, rather than a second time as a row the ledger does not hold.
struct Race {
    /// The id both submissions of this race derive, when the backend showed
    /// the identity is in the store. `None` when neither outcome did.
    seeded: Option<(&'static str, Uuid)>,
    /// What the race observed, and what the check required instead.
    violations: Vec<ContractViolation>,
}

/// `dedup-concurrent` — *"Identical and divergent pairs on one identity,
/// driven through several gateway replicas before convergence.
/// `linearizable`: the identical pair is absorbed, and the divergent pair
/// yields one acceptance and one `IdempotencyConflict`. `eventual`: only the
/// first write in commit order ever shows on any read, figure, or feed page,
/// and the divergent pair counts one collision. At either level, an insert
/// reported `Transient` that commits after convergence on a divergent retry,
/// despite an earlier `accepted_at`, changes nothing."* (DESIGN §3.3,
/// "Plugin contract tests", line 1319.)
///
/// **That row is a pointer rather than the rule.** The rule is DESIGN §3.1's
/// "Dedup level" invariant, and the clauses this check rests on are these:
///
/// * *"Races under one identity resolve by the store's **commit order** —
///   the order the plugin's own state makes writes durable in, never a
///   gateway timestamp. The first write in commit order is the
///   **survivor**, and every read path, fold, reconciliation figure,
///   materialised aggregate, and the feed show it and nothing else; a read
///   before convergence may show a write that proves not to be first, never
///   two."*
/// * *"An identity **converges** once no earlier write can still become
///   visible, the survivor is visible to every dedup check the plugin runs,
///   and no persist call that missed it is still to return; the plugin
///   establishes this from its commit or replication state, never from
///   elapsed time, within a declared **convergence bound**; converging later
///   is a conformance defect."*
/// * *"From then until retention frees the identity, no later write
///   displaces the survivor and no outcome returned accepts divergent
///   content: an identical write is absorbed, a divergent one is
///   `IdempotencyConflict`."*
/// * *"Before convergence the declared level applies. `linearizable`: the
///   bound is zero, and every write is decided as it commits. `eventual`: a
///   write can be acknowledged and then discarded, each divergent discard
///   counted; the feed never returns it, and an aggregate or reconciliation
///   figure that counted it recomputes."*
///
/// # A replica is modelled as a concurrent task, and why that is faithful
///
/// DESIGN's row drives its pairs *"through several gateway replicas"*. The
/// reference backend is a single in-process instance behind a mutex, and
/// this check reaches it the only way the SPI offers: two futures over one
/// `&dyn UsageCollectorPluginV1` handle, driven together by
/// `toolkit::tokio::join!`.
///
/// **That is a faithful reduction, and the argument belongs here rather than
/// in a reader's head.** Every rule quoted above is a property of the
/// *store's own serialisation* — commit order, a survivor, an identity that
/// converges, a later write that must not displace. None of them mentions
/// the caller, and none of them can: a plugin cannot see how many processes
/// its submissions came from, and DESIGN gives it nothing to key on if it
/// could. Two interleaved submissions against one dedup identity are two
/// interleaved submissions whether they arrive from two tasks or two
/// replicas, and the plugin's obligation is identical in both. What several
/// replicas add over several tasks is *where the interleaving comes from*,
/// not *what the backend owes*.
///
/// **How much the two futures actually interleave is the backend's to
/// decide, and that is the point rather than a weakness.** `join!` drives
/// both on one task, so they hand off to each other at their own await
/// points: a plugin whose write goes to a database suspends there and the
/// second submission really is in flight while the first is, and a plugin
/// that decides a write inside a synchronous critical section never
/// suspends and is serialised by construction. The second is not evading
/// the check. Serialising concurrent submissions under one identity is
/// exactly what *"Races under one identity resolve by the store's **commit
/// order**"* asks for, and a backend that achieves it by holding a lock has
/// met the obligation rather than dodged it. What this check asserts is the
/// outcome of the resolution, which is the same question either way.
///
/// What the reduction does not buy is worth saying in the same breath. It
/// does not exercise a plugin's own cross-process coordination — a
/// distributed lock, a leader election, a replication wait — because
/// nothing dispatched from one process can. A backend whose serialisation
/// holds within a process and fails across one passes this check, and the
/// obligation it breaks is the one DESIGN puts on the plugin's *"commit or
/// replication state"*. That is the same class of thing
/// [`converged_target_lookup`](super::converged_target_lookup())'s docs
/// record about the bound it cannot time: stated by DESIGN, not reachable
/// from a suite that drives a plugin through its SPI.
///
/// # What is asserted
///
/// Two identities are raced, one per pair, each under its own idempotency
/// key so that no outcome of one can be explained by the other:
///
/// 1. **The identical pair.** Two submissions of one entry, byte for byte
///    the same in every caller-supplied field, dispatched together. Both
///    answer `Ok` carrying that entry. **This holds at either level**, and
///    that is not a widening of DESIGN's `linearizable` clause: two
///    identical writes have no content to disagree about, so an absorb and
///    a second acknowledgement are indistinguishable — both answer with an
///    entry equal in every caller-supplied field, which is the same
///    argument `dedup-floor` records about its identical in-batch pair. What
///    the assertion forbids at either level is a **refusal**, which reports
///    a conflict between a caller and itself over content the two agree on.
/// 2. **The divergent pair, under [`DedupLevel::Linearizable`].** Two
///    submissions sharing all six identity inputs and differing in the
///    quantity, dispatched together. Exactly one answers `Ok` and the other
///    is [`UsageCollectorPluginError::IdempotencyConflict`] — *"the bound is
///    zero, and every write is decided as it commits"* — and the conflict's
///    `existing` is the entry that was accepted, which is how the losing
///    caller learns what its key is now bound to.
/// 3. **The survivor is not displaced, under
///    [`DedupLevel::Linearizable`].** A point read after the race answers
///    the accepted entry's content. This is the assertion no outcome above
///    can make: a backend can hand both callers the conforming answer and
///    still write the loser's content over the winner's row, and *"no later
///    write displaces the survivor"* is the clause that forbids it. Nothing
///    else in the suite reads a raced identity's content back.
/// 4. **Only one write shows, on three surfaces, under
///    [`DedupLevel::Eventual`].** After sleeping the declared convergence
///    bound, a `list_usage_records` page holds one row per raced identity, a
///    `COUNT` fold over the same range reports one term per identity, and a
///    feed page over this check's own subscription delivers one entry per
///    identity — and the entry the feed delivers is the same write the
///    ledger page holds. Three surfaces rather than one because DESIGN names
///    them separately: *"every read path, fold, reconciliation figure,
///    materialised aggregate, and the feed show it and nothing else"*. A
///    backend that converged its ledger but not its feed breaks exactly that
///    clause and nothing else.
///
/// Under `Eventual` the acceptances themselves are not asserted, and the
/// quoted clause is why: *"a write can be acknowledged and then discarded"*,
/// so two acknowledgements are admissible there and only the post-bound
/// reads decide anything.
///
/// # Three clauses stated here and exercised nowhere
///
/// A green run must not be read as covering these. Each is DESIGN's, each is
/// out of this suite's reach, and none is omitted quietly.
///
/// * **The `Transient`-then-commits clause.** DESIGN's row closes: *"At
///   either level, an insert reported `Transient` that commits after
///   convergence on a divergent retry, despite an earlier `accepted_at`,
///   changes nothing."* Asserting it needs a backend that can be told to
///   report `Transient` to its caller and commit the write anyway, on a
///   schedule the check chooses. The SPI has no such input, and inventing
///   one means a defect switch — a flag, a hook, a `#[cfg(test)]` branch —
///   inside [`reference`](crate::contract::reference), which is the
///   exemplar a plugin author copies and which deliberately carries none.
///   So the clause is stated and not driven. What a conforming backend owes
///   is unchanged by that: the late commit is *"A write whose caller was
///   already answered, such as a timed-out insert that commits late, is
///   discarded, counted when divergent."*
/// * **The collision count.** The row's `eventual` half ends *"and the
///   divergent pair counts one collision"*, and §3.1 has *"each divergent
///   discard counted"*. Those are published metrics (§3.10), and
///   [`UsageCollectorPluginV1`] declares seven methods, none of which
///   reports one. A behavioural suite cannot read a counter.
/// * **Which write won, under `Eventual`.** DESIGN names the survivor as
///   *"the first write in commit order"*, and under an `eventual`
///   declaration a conforming backend may acknowledge both, so nothing this
///   check can read says which committed first. Probe 4 therefore asserts
///   the observable projection of the clause — one write, and the same one
///   on all three surfaces — and not the identity of the first committer.
///   Under `linearizable` the outcomes do name it, which is what probes 2
///   and 3 rest on.
///
/// # Which of these assertions any subject reaches
///
/// Measured by neutering each in turn, not reasoned:
///
/// * **Probe 3 is individually load-bearing**, and
///   `contract_mutants`'s `Defect::ADivergentWriteDisplacesTheSurvivor` is
///   the subject built for it. Neutering probe 3 leaves that subject's row
///   empty. It is the only assertion in the suite that subject reaches: it
///   answers every collision the conforming way and only the stored row
///   moves, so `dedup-floor`'s row count and `COUNT` (which read identity,
///   never content) and `at-most-one-invalidation`'s conflict shape are all
///   satisfied by it.
/// * **Probe 4's three surfaces are load-bearing together and none alone.**
///   `Defect::LedgerHasNoUniqueConstraint` stores both writes of a raced
///   identity, so the page, the fold and the feed each report; neutering any
///   one leaves the other two reporting and that subject's `Eventual` row
///   unchanged, and neutering all three takes this check out of it. The
///   fourth assertion in probe 4 — that the feed and the page show the *same*
///   write — is reached by no subject: see the gap recorded below.
/// * **Probes 1 and 2 are reached by no subject**, and the gaps are the two
///   `dedup-floor` already records rather than new ones. Probe 1's is that
///   no subject refuses an identical re-delivery of a *record*;
///   `Defect::RefusesAWithdrawalWithTheSameReason` refuses only
///   invalidations, and this check submits none. Probe 2's is the one
///   `Defect::LedgerHasNoUniqueConstraint`'s docs name: the subject that
///   would close it absorbs a divergent re-delivery of a record, and
///   `Defect::AbsorbsAWithdrawalWithAnotherReason` never reaches one. Such a
///   subject would fail `dedup-floor`'s divergent submission too, so it
///   would widen that check's row rather than isolate here.
/// * **Probe 4's cross-surface assertion is a recorded gap.** The subject
///   that would close it is a backend whose feed answers a different write
///   of one identity from its ledger page. That is the same *shape* of
///   defect `Defect::LedgerHasNoUniqueConstraint`'s docs already record as
///   open — *"Isolating one from the other needs a subject whose fold
///   disagrees with its own ledger page"* — one surface disagreeing with
///   another, in the third place rather than the second. It stays because
///   the clause is DESIGN's own and this is the only check that reads the
///   feed for it: without the assertion, a backend that converged its ledger
///   and not its feed would meet nothing here at all.
pub async fn dedup_concurrent(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let fixtures = match dedup_concurrent_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{DEDUP_CONCURRENT}` fixtures, so \
                     nothing was submitted. This is a fault in the suite, not in the plugin under \
                     test: {detail}"
                ),
            )];
        }
    };

    let mut violations = Vec::new();
    let mut seeded: Vec<(&'static str, Uuid)> = Vec::new();
    for race in [
        the_identical_pair_is_absorbed(plugin, &fixtures).await,
        the_divergent_pair_resolves_once(plugin, &fixtures, level).await,
    ] {
        violations.extend(race.violations);
        if let Some(identity) = race.seeded {
            seeded.push(identity);
        }
    }

    if let DedupLevel::Eventual { convergence_bound } = level {
        toolkit::tokio::time::sleep(convergence_bound).await;
        violations.extend(only_one_write_shows_on_every_surface(plugin, &fixtures, &seeded).await);
    }
    violations
}

/// Probe one: two identical submissions of one identity, raced, both answer
/// with that entry.
///
/// The comparison is against the fixture rather than against either outcome,
/// which is the point of an identical pair: whichever of the two the backend
/// made durable, the entry it answers with carries this content.
async fn the_identical_pair_is_absorbed(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
) -> Race {
    let entry = &fixtures.identical;
    let (left, right) = toolkit::tokio::join!(
        plugin.create_usage_record(fixtures.identical.clone()),
        plugin.create_usage_record(fixtures.identical_again.clone()),
    );

    let mut violations = Vec::new();
    for (replica, outcome) in [("first", &left), ("second", &right)] {
        if matches!(outcome, Ok(stored) if is_the_stored(stored, entry)) {
            continue;
        }
        violations.push(violation(
            DEDUP_CONCURRENT,
            format!(
                "two submissions of record {id} carrying identical caller-supplied content were \
                 dispatched together against one identity, and the {replica} of them answered \
                 {outcome:?}. Both answer with that entry. The two have no content to disagree \
                 about, so whichever of them the store made durable first is the survivor and the \
                 other is absorbed against it - and an absorb and a second acknowledgement are \
                 indistinguishable here, which is why either is accepted. What is not is a \
                 refusal: an `IdempotencyConflict` between a caller and itself, over content the \
                 two submissions agree on, is a collision resolved by something other than exact \
                 equality of the caller-supplied fields.",
                id = entry.id,
            ),
        ));
    }

    Race {
        seeded: identity_reached_the_store(&[&left, &right], entry.id)
            .then_some(("the identical pair", entry.id)),
        violations,
    }
}

/// Probes two and three: two divergent submissions of one identity, raced.
///
/// Under [`DedupLevel::Linearizable`] the outcomes are asserted and the
/// stored row is read back. Under [`DedupLevel::Eventual`] neither is:
/// *"a write can be acknowledged and then discarded"*, so two
/// acknowledgements are admissible and no outcome names the survivor, which
/// leaves the post-bound surfaces in [`only_one_write_shows_on_every_surface`]
/// as the whole of what that declaration can be held to.
async fn the_divergent_pair_resolves_once(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
    level: DedupLevel,
) -> Race {
    let id = fixtures.divergent.id;
    let (left, right) = toolkit::tokio::join!(
        plugin.create_usage_record(fixtures.divergent.clone()),
        plugin.create_usage_record(fixtures.divergent_other.clone()),
    );
    let seeded =
        identity_reached_the_store(&[&left, &right], id).then_some(("the divergent pair", id));

    if !matches!(level, DedupLevel::Linearizable) {
        return Race {
            seeded,
            violations: Vec::new(),
        };
    }

    let ((Ok(accepted), Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }))
    | (Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }), Ok(accepted))) =
        (&left, &right)
    else {
        return Race {
            seeded,
            violations: vec![violation(
                DEDUP_CONCURRENT,
                format!(
                    "two submissions of record {id}, sharing all six identity inputs and \
                     carrying the quantities `{first}` and `{second}`, were dispatched together \
                     against one identity under a `linearizable` declaration, and they answered \
                     {left:?} and {right:?}. That declaration is a zero convergence bound with \
                     every write decided as it commits, so the race resolves in the store's own \
                     commit order: the first write in commit order is the survivor and is \
                     accepted, and the second carries divergent content against a converged \
                     identity and is `IdempotencyConflict`. Two acceptances are two callers each \
                     told their content was stored, of which at most one can be true; two \
                     conflicts leave the identity holding neither submission, or holding one \
                     whose caller was told it was refused.",
                    first = fixtures.divergent.quantity,
                    second = fixtures.divergent_other.quantity,
                ),
            )],
        };
    };

    let mut violations = Vec::new();
    if !is_the_stored(existing, accepted) {
        violations.push(violation(
            DEDUP_CONCURRENT,
            format!(
                "the raced divergent pair of record {id} resolved into one acceptance and one \
                 `IdempotencyConflict`, and the conflict named {named} where the acceptance \
                 answered {won}. The `existing` a conflict carries is the entry that won the \
                 identity, which is how the losing caller learns what its key is already bound to; \
                 naming anything else hands it an entry no submission of this race produced, or \
                 the very content the store refused.",
                named = rendered(existing),
                won = rendered(accepted),
            ),
        ));
    }
    violations.extend(the_survivor_is_not_displaced(plugin, accepted).await);

    Race { seeded, violations }
}

/// Probe three: after the race, a read answers the entry that was accepted.
///
/// **This is the assertion no outcome above can make.** A backend can answer
/// both callers exactly as a conforming one would - the winner its own
/// entry, the loser an `IdempotencyConflict` naming that entry - and then
/// write the loser's content over the winner's row, which an `ON CONFLICT
/// ... DO UPDATE` on the dedup identity does. Both callers were told the
/// truth and the ledger holds the other submission.
///
/// The point read rather than a range: this probe is about one identity's
/// content, and a page would couple it to the period rule and to whatever
/// else the range holds. The comparison is [`is_the_stored`], the id
/// together with every caller-supplied field, because the id alone cannot
/// see a displacement - the two racers derive one id by construction.
async fn the_survivor_is_not_displaced(
    plugin: &dyn UsageCollectorPluginV1,
    accepted: &UsageRecord,
) -> Vec<ContractViolation> {
    let outcome = plugin
        .get_usage_record(accepted.id, &contract_scope(), false)
        .await;
    if matches!(&outcome, Ok(stored) if is_the_stored(stored, accepted)) {
        return Vec::new();
    }
    vec![violation(
        DEDUP_CONCURRENT,
        format!(
            "record {id} was raced by two divergent submissions, the plugin accepted one of them \
             and answered the other with an `IdempotencyConflict` naming it, and a read straight \
             afterwards answered {observed}. The accepted entry is the survivor, and from \
             convergence until retention frees the identity no later write displaces it: the \
             divergent submission the plugin refused is not written, not merged, and not allowed \
             to refresh the row. A backend that answers both callers correctly and then updates \
             the row on conflict passes every other assertion about this race and holds the \
             content it told a caller it would not store.",
            id = accepted.id,
            observed = match &outcome {
                Ok(stored) => format!(
                    "{}, and it accepted {}",
                    rendered(stored),
                    rendered(accepted)
                ),
                Err(err) => format!(
                    "`{err}`, and the entry it accepted was {}",
                    rendered(accepted)
                ),
            },
        ),
    )]
}

/// Probe four: after the declared convergence bound, one write of each raced
/// identity shows on the ledger page, in the `COUNT` fold and on the feed -
/// and the feed and the page show the same one.
///
/// Four assertions over three reads, each reported on its own. The reads are
/// taken once and shared: a page that could not be read is reported as one
/// failure rather than as a premise missing from three separate messages.
async fn only_one_write_shows_on_every_surface(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    let page = match dedup_concurrent_page(plugin, fixtures).await {
        Ok(items) => Some(items),
        Err(detail) => {
            violations.push(violation(DEDUP_CONCURRENT, detail));
            None
        }
    };
    if let Some(items) = page.as_deref() {
        violations.extend(the_ledger_holds_one_write_per_identity(items, seeded));
    }
    violations.extend(the_fold_counts_one_per_identity(plugin, fixtures, seeded).await);

    let feed = match dedup_concurrent_feed(plugin, fixtures).await {
        Ok(entries) => Some(entries),
        Err(detail) => {
            violations.push(violation(DEDUP_CONCURRENT, detail));
            None
        }
    };
    if let Some(entries) = feed.as_deref() {
        violations.extend(the_feed_delivers_one_write_per_identity(entries, seeded));
        if let Some(items) = page.as_deref() {
            violations.extend(the_feed_and_the_ledger_agree(items, entries, seeded));
        }
    }
    violations
}

/// The ledger surface of probe four: one row per raced identity, and no row
/// besides.
fn the_ledger_holds_one_write_per_identity(
    items: &[UsageRecord],
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let observed = rows_per_identity(items, seeded);
    let each_once = seeded
        .iter()
        .all(|(_, id)| items.iter().filter(|item| item.id == *id).count() == 1);
    if each_once && items.len() == seeded.len() {
        return Vec::new();
    }
    vec![violation(
        DEDUP_CONCURRENT,
        format!(
            "this check raced {identities} identities over the range `[{from}, {to})`, two \
             submissions each, and after the whole of the declared convergence bound \
             `list_usage_records` came back with {rows} row(s): {observed:?}. Only the first write \
             in commit order shows on a read path, and a read after convergence shows it and \
             nothing else. A read *before* convergence may show a write that proves not to be \
             first - never two - and this read is after the bound the plugin declared, so a second \
             row for one identity is a race the store never resolved rather than one it has not \
             resolved yet.",
            from = DEDUP_CONCURRENT_WINDOW_FROM,
            to = DEDUP_CONCURRENT_WINDOW_TO,
            identities = seeded.len(),
            rows = items.len(),
        ),
    )]
}

/// The fold surface of probe four: a `COUNT` over the same range reports one
/// term per raced identity.
///
/// A second surface rather than a restatement of the first. DESIGN puts the
/// survivor on *"every read path, fold, reconciliation figure, materialised
/// aggregate, and the feed"*, and a fold is not served from the ledger page:
/// a backend whose `COUNT` runs against a materialised aggregate, or against
/// a continuous aggregate refreshed on insert, can hold one row on the
/// ledger and count both writes of the race.
///
/// `COUNT` rather than `SUM` because it reads no quantity: the divergent
/// pair's two racers differ from each other exactly in the quantity, so a
/// `SUM` here would report a second surviving write and a differently
/// resolved race as the same number and could not tell them apart.
async fn the_fold_counts_one_per_identity(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let result = match plugin
        .query_aggregated_usage_records(
            fixtures.meter.clone(),
            fixtures.range,
            AggregationFold::Count,
            &contract_query(DEDUP_CONCURRENT_PAGE_LIMIT),
            &[],
            &[],
        )
        .await
    {
        Ok(result) => result,
        Err(err) => {
            return vec![violation(
                DEDUP_CONCURRENT,
                format!(
                    "the `COUNT` over the range holding this check's raced identities failed, so \
                     how many writes of a race a fold counts could not be decided: {err}"
                ),
            )];
        }
    };

    let expected = BigDecimal::from(u64::try_from(seeded.len()).unwrap_or(u64::MAX));
    let counted: Vec<Option<&BigDecimal>> = result
        .buckets
        .iter()
        .map(|bucket| bucket.value.as_ref())
        .collect();
    if counted.len() == 1 && counted.first().copied().flatten() == Some(&expected) {
        return Vec::new();
    }
    vec![violation(
        DEDUP_CONCURRENT,
        format!(
            "an ungrouped `COUNT` over the range `[{from}, {to})` reported {counted:?} after the \
             declared convergence bound, and this check raced {identities} identities there, none \
             of them withdrawn. One write of a race survives and a fold counts it once. The ledger \
             page is a different surface from the fold and a backend answering one of them \
             correctly says nothing about the other: DESIGN puts the survivor on every read path, \
             fold, reconciliation figure, materialised aggregate and the feed, so an aggregate \
             that counted a discarded write has to recompute.",
            from = DEDUP_CONCURRENT_WINDOW_FROM,
            to = DEDUP_CONCURRENT_WINDOW_TO,
            identities = seeded.len(),
        ),
    )]
}

/// The feed surface of probe four: one entry per raced identity.
///
/// Its own assertion rather than a third phrasing of the ledger page's,
/// because the feed is its own surface and DESIGN names it beside the read
/// paths and the folds. It is also the surface a charging consumer reads
/// *instead of* `list_usage_records`, so a race the feed never resolved is a
/// measurement rated twice.
fn the_feed_delivers_one_write_per_identity(
    entries: &[UsageRecord],
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let observed = rows_per_identity(entries, seeded);
    let each_once = seeded
        .iter()
        .all(|(_, id)| entries.iter().filter(|entry| entry.id == *id).count() == 1);
    if each_once {
        return Vec::new();
    }
    vec![violation(
        DEDUP_CONCURRENT,
        format!(
            "a feed read over a subscription naming this check's own meter delivered \
             {delivered} entries after the whole of the declared convergence bound, of which \
             {observed:?}. The feed shows the survivor of a race and nothing else - it is named \
             beside the read paths and the folds, and `eventual` says of an acknowledged write \
             that is later discarded that the feed never returns it. A consumer rates what the \
             feed hands it, so a race the feed never resolved is a measurement charged twice.",
            delivered = entries.len(),
        ),
    )]
}

/// The cross-surface assertion of probe four: the write the feed delivers
/// for an identity is the write the ledger page holds for it.
///
/// **This is the assertion the per-surface counts cannot make.** A backend
/// that converged its ledger and not its feed shows one entry on each - so
/// both counts are right - and shows two different writes of the same race.
/// DESIGN admits exactly one survivor for all of them: *"every read path,
/// fold, reconciliation figure, materialised aggregate, and the feed show it
/// and nothing else"*.
///
/// An identity absent from either surface is passed over here. The counts
/// above already report it, and reporting it again would read as two defects
/// rather than one.
fn the_feed_and_the_ledger_agree(
    items: &[UsageRecord],
    entries: &[UsageRecord],
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for (role, id) in seeded {
        let (Some(stored), Some(delivered)) = (
            items.iter().find(|item| item.id == *id),
            entries.iter().find(|entry| entry.id == *id),
        ) else {
            continue;
        };
        if stored.caller_supplied_eq(delivered) {
            continue;
        }
        violations.push(violation(
            DEDUP_CONCURRENT,
            format!(
                "{role} (record {id}) was raced by two submissions, and after the declared \
                 convergence bound the ledger page answered {stored} while the feed delivered \
                 {delivered}. One of those is the survivor and the other is a write the store was \
                 meant to discard: an identity has one first write in commit order, and every read \
                 path, fold, reconciliation figure, materialised aggregate and the feed show it \
                 and nothing else. Each surface holding one entry is not enough - a backend that \
                 converged its ledger and not its feed satisfies both counts and hands a consumer \
                 content no reader of the ledger will ever see.",
                stored = rendered(stored),
                delivered = rendered(delivered),
            ),
        ));
    }
    violations
}

/// How many rows each raced identity took on one surface, rendered for a
/// report.
fn rows_per_identity(rows: &[UsageRecord], seeded: &[(&'static str, Uuid)]) -> Vec<String> {
    seeded
        .iter()
        .map(|(role, id)| {
            let count = rows.iter().filter(|row| row.id == *id).count();
            format!("{role} ({id}) on {count} row(s)")
        })
        .collect()
}

/// Whether either outcome of a race shows the identity is in the store.
///
/// An `Ok` carrying it does, and so does an `IdempotencyConflict` whose
/// `existing` carries it: a conflict is answered against a stored entry, so
/// the identity is seeded even though this submission was not. Both shapes
/// count because the `Eventual` half counts rows for the identities that
/// really were seeded, and a race whose outcomes say nothing about the store
/// must not have its rows counted at all.
fn identity_reached_the_store(
    outcomes: &[&Result<UsageRecord, UsageCollectorPluginError>],
    id: Uuid,
) -> bool {
    outcomes.iter().any(|outcome| match outcome {
        Ok(stored) => stored.id == id,
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => existing.id == id,
        Err(_) => false,
    })
}

/// Whether `stored` is `expected` as the plugin would answer with it.
///
/// The id **together with** every caller-supplied field
/// ([`UsageRecord::caller_supplied_eq`]) rather than either alone, and both
/// halves earn their place here more than anywhere else in the suite: this
/// check's two racers of an identity derive **one** id by construction, so
/// the id alone cannot tell the survivor from the write that lost, which is
/// the whole of what probes two and three are about. The fields alone would
/// admit an answer carrying the right content under some other entry's id,
/// which two identities on one meter exist to keep apart.
///
/// Whole-record equality is deliberately not used. It also reads
/// `accepted_at` and `origin`, which are server-assigned and no part of this
/// check's rule - a backend stamping its own instant would be reported here,
/// under a message about a race, while the check built for that rule
/// reported it too.
fn is_the_stored(stored: &UsageRecord, expected: &UsageRecord) -> bool {
    stored.id == expected.id && stored.caller_supplied_eq(expected)
}

/// One entry's identity and the caller-supplied field this check's racers
/// differ in, rendered for a report.
///
/// The quantity is named rather than the whole record: the two racers of the
/// divergent pair agree on everything else by construction, so it is the one
/// field that says which of them a surface answered with. Naming the id
/// alone would render the two identically, which is the whole difficulty
/// this check's reports have to get past.
fn rendered(record: &UsageRecord) -> String {
    format!(
        "record {id} carrying the quantity `{quantity}`",
        id = record.id,
        quantity = record.quantity,
    )
}

/// Every entry the range under test comes back with on the ledger path.
///
/// A `Vec` rather than a set, because the count is the assertion: a set
/// would collapse the second surviving write this surface exists to find.
///
/// `Err` carries a ready-to-report detail.
async fn dedup_concurrent_page(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
) -> Result<Vec<UsageRecord>, String> {
    let page = plugin
        .list_usage_records(
            fixtures.meter.clone(),
            fixtures.range,
            &contract_query(DEDUP_CONCURRENT_PAGE_LIMIT),
            &[],
        )
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range holding this check's raced \
                 identities, so how many writes of a race a read path shows could not be decided: \
                 {err}"
            )
        })?;
    Ok(page.items)
}

/// Every entry the feed delivers for this check's own meter, following the
/// cursor until it stands still.
///
/// The subscription is this check's own meter alone, which is what
/// [`check_meter`] exists for: a feed read selects by meter, so a
/// subscription naming the suite's shared meter would carry whatever the
/// other checks left on it and what this check observed would turn on
/// dispatch order. One meter carries both raced identities, and a second
/// role would buy nothing - the assertions are per identity, and a page
/// holding both is one read rather than two.
///
/// The scope is [`contract_scope`], the same single-tenant compiled scope
/// the ledger path dispatches. Every entry this check stores is inside it,
/// so the read buys shape coverage on the path and no scope *enforcement*,
/// for the reason
/// [`SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`](crate::contract::SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH)
/// gives.
///
/// `Err` carries a ready-to-report detail.
async fn dedup_concurrent_feed(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
) -> Result<Vec<UsageRecord>, String> {
    let subscription = [fixtures.meter.clone()];
    let scope = contract_scope();
    let mut delivered: Vec<UsageRecord> = Vec::new();
    let mut start = FeedStart::Oldest;
    let mut reached: Option<FeedPosition> = None;
    for _ in 0..DEDUP_CONCURRENT_FEED_PAGES {
        let page = plugin
            .read_feed_page(
                &subscription,
                &scope,
                start,
                None,
                DEDUP_CONCURRENT_PAGE_LIMIT,
            )
            .await
            .map_err(|err| {
                format!(
                    "`read_feed_page` failed over a subscription naming this check's own meter, \
                     so how many writes of a race the feed shows could not be decided: {err}"
                )
            })?;
        delivered.extend(page.entries);
        // An unbounded read carries a cursor on every page; `None` is the
        // shape a bounded replay returns at its `until`, and this read sends
        // none. Either way there is nothing left to follow.
        let Some(next) = page.next else { break };
        if reached.as_ref() == Some(&next) {
            break;
        }
        reached = Some(next.clone());
        start = FeedStart::After(next);
    }
    Ok(delivered)
}

/// Builds the identical pair, the divergent pair, and the range the
/// `Eventual` half reads them back over.
///
/// The meter is this check's own rather than the suite's shared one, for two
/// independent reasons. [`run_all`](crate::contract::run_all) dispatches
/// every check against one persistent backend that never removes an entry,
/// and this check counts the rows a range returns, so it reads a meter
/// nothing else writes to; and its feed read selects by meter, so a
/// subscription naming the shared meter would carry whatever the other
/// checks left on it.
///
/// **Each idempotency key is submitted over one covered period, and that is
/// a decision rather than an accident**, for the reason
/// `dedup-floor`'s own fixtures give:
/// `contract_mutants`'s `Defect::DedupIgnoresThePeriod` keys an index on the
/// derived identity with its two period bounds struck out, and a check
/// reusing one key across two periods would hand that subject a collision it
/// must refuse and appear in its matrix row for a rule that is not this
/// one's.
fn dedup_concurrent_fixtures() -> Result<DedupConcurrentFixtures, String> {
    let meter = check_meter(DEDUP_CONCURRENT, "main")?;
    let quantity = UsageQuantity::parse(DEDUP_CONCURRENT_QUANTITY).map_err(|err| {
        format!("the check's own quantity `{DEDUP_CONCURRENT_QUANTITY}` does not parse: {err}")
    })?;
    let divergent_quantity =
        UsageQuantity::parse(DEDUP_CONCURRENT_DIVERGENT_QUANTITY).map_err(|err| {
            format!(
                "the check's own divergent quantity `{DEDUP_CONCURRENT_DIVERGENT_QUANTITY}` does \
                 not parse: {err}"
            )
        })?;

    let identical_key = dedup_concurrent_key("identical")?;
    let divergent_key = dedup_concurrent_key("divergent")?;
    let entry = |key: &IdempotencyKey, quantity: UsageQuantity| {
        fixture_record_on(
            meter.clone(),
            CONTRACT_TENANT_ID,
            key,
            quantity,
            CONTRACT_ACCEPTED_AT,
            DEDUP_CONCURRENT_WINDOW_FROM,
            DEDUP_CONCURRENT_WINDOW_END,
        )
    };

    let range = TimeRange::new(DEDUP_CONCURRENT_WINDOW_FROM, DEDUP_CONCURRENT_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let identical = entry(&identical_key, quantity)?;
    let identical_again = entry(&identical_key, quantity)?;
    let divergent = entry(&divergent_key, quantity)?;
    let divergent_other = entry(&divergent_key, divergent_quantity)?;

    let fixtures = DedupConcurrentFixtures {
        meter,
        range,
        identical,
        identical_again,
        divergent,
        divergent_other,
    };
    fixtures.guards()?;
    Ok(fixtures)
}

impl DedupConcurrentFixtures {
    /// The four facts this check's assertions read, established rather than
    /// assumed. All four are the suite's own rather than the plugin's, so
    /// all four are reported as [`HARNESS_FAULT`] rather than against the
    /// plugin.
    ///
    /// * **The identical pair is one identity.** Both racers derive one id,
    ///   so the two submissions collide rather than pass each other. Two ids
    ///   would be two entries colliding on nothing, every backend would
    ///   accept both, and probe one would assert nothing at all.
    /// * **The identical pair is genuinely identical**, in every field
    ///   [`UsageRecord::caller_supplied_eq`] reads. That is a fact about the
    ///   builder rather than about the language here, because the two are
    ///   built by two calls rather than cloned: a builder that varied
    ///   anything between them would turn probe one's race into the
    ///   divergent one, and a conforming backend would then answer
    ///   `IdempotencyConflict` where probe one requires the stored entry.
    /// * **The divergent pair is one identity and really diverges.** The
    ///   quantity is not one of the six identity inputs, so a pair differing
    ///   only there is one identity submitted twice; and a collision
    ///   resolves by exact equality of the caller-supplied fields, so a
    ///   "divergent" racer equal in all of them is an ordinary absorbed
    ///   retry and a conforming backend would answer `Ok` where probe two
    ///   requires a conflict.
    /// * **The two identities are two.** The `Eventual` half counts one row
    ///   per identity on three surfaces, so two races sharing an id would
    ///   make every expected count wrong - and would put both races'
    ///   submissions on one identity, where the second race's racers would
    ///   resolve against the first's rather than against each other.
    fn guards(&self) -> Result<(), String> {
        if self.identical.id != self.identical_again.id {
            return Err(format!(
                "the two racers of the identical pair derive {left} and {right}; they are meant to \
                 be one identity submitted twice at once, and two entries under two ids collide on \
                 nothing, so every backend would accept both and the absorb this check requires \
                 would never be reachable",
                left = self.identical.id,
                right = self.identical_again.id,
            ));
        }
        if !self.identical.caller_supplied_eq(&self.identical_again) {
            return Err(format!(
                "the two racers of the identical pair of record {id} differ in a caller-supplied \
                 field; a collision resolves by exact equality of those fields, so this pair is a \
                 divergent one and a conforming backend answers `IdempotencyConflict` where this \
                 check requires the stored entry",
                id = self.identical.id,
            ));
        }
        if self.divergent.id != self.divergent_other.id {
            return Err(format!(
                "the two racers of the divergent pair derive {left} and {right}; the quantity is \
                 not one of the six inputs to the derived identity, so a pair differing only there \
                 is the same identity - and two entries under two ids collide on nothing, so every \
                 backend would accept both and the conflict this check requires would never be \
                 reachable",
                left = self.divergent.id,
                right = self.divergent_other.id,
            ));
        }
        if self.divergent.caller_supplied_eq(&self.divergent_other) {
            return Err(format!(
                "the two racers of the divergent pair of record {id} are equal in every \
                 caller-supplied field; a collision resolves by exact equality of those fields, so \
                 this pair is an identical one and a conforming backend absorbs the second racer \
                 where this check requires `IdempotencyConflict`",
                id = self.divergent.id,
            ));
        }
        if self.identical.id == self.divergent.id {
            return Err(format!(
                "this check's two races seed one identity ({id}) rather than two; the `Eventual` \
                 half counts one row per identity on three surfaces, and two races sharing an \
                 identity would both make those counts wrong and have the second race's racers \
                 resolve against the first race's entry instead of against each other",
                id = self.identical.id,
            ));
        }
        Ok(())
    }
}

/// The idempotency key one race of this check submits under.
///
/// Keyed on the race name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's.
fn dedup_concurrent_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{DEDUP_CONCURRENT}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
