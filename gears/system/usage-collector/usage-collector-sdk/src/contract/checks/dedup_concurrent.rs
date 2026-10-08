//! The DESIGN §3.3 `dedup-concurrent` check.
//!
//! See [`dedup_concurrent`] for what it asserts, including why driving two
//! concurrent tasks at one backend handle is a faithful reduction of
//! DESIGN's "several gateway replicas" and which of the row's clauses this
//! check states without exercising.

use bigdecimal::BigDecimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, check_meter, check_window_from, contract_query,
    contract_scope, fixture_record_on, seed_usage_record, violation,
};
use crate::contract::{ContractViolation, DEDUP_CONCURRENT, DedupLevel, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPosition, FeedStart};
use crate::models::{AggregationFold, IdempotencyKey};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::stored::{MeterRef, StoredUsageRecord};
use crate::time_range::TimeRange;

/// The start of the covered period every entry of this check carries, and
/// the inclusive lower bound of the range its `Eventual` half reads them
/// back over.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives. It matters here because the `Eventual` half counts
/// the rows a range returns against the identities this check raced, so a
/// stray entry inside it would read as a row no identity accounts for — the
/// shape of the failure that half exists to report. This check also reads
/// over a meter of its own, so range and meter are independent reasons
/// nothing else can land in what it counts.
const DEDUP_CONCURRENT_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(DEDUP_CONCURRENT, "main");

/// The end of that covered period.
///
/// **Every entry of this check carries one period**, for the reason
/// `dedup-floor`'s own period end gives: the identities here are separated by
/// their idempotency keys alone, so `contract_mutants`'s
/// `Defect::DedupIgnoresThePeriod` — an index on the derived identity with the
/// period bounds struck out — answers exactly as the full identity does for
/// everything this check submits.
const DEDUP_CONCURRENT_WINDOW_END: time::OffsetDateTime =
    DEDUP_CONCURRENT_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The exclusive upper bound of the range the `Eventual` half reads over, an
/// hour past [`DEDUP_CONCURRENT_WINDOW_END`].
///
/// The range holds **both** covered-period bounds, for the reason
/// `dedup-floor`'s own upper bound gives: this check asserts nothing about
/// period selection, and a range beginning at
/// [`DEDUP_CONCURRENT_WINDOW_END`] would make its read surfaces unanswerable
/// for a backend selecting on `window_start`, coupling this check to
/// `window-end-selection`'s rule for no reason of its own.
const DEDUP_CONCURRENT_WINDOW_TO: time::OffsetDateTime =
    DEDUP_CONCURRENT_WINDOW_END.saturating_add(time::Duration::hours(1));

/// The quantity both racers of the identical pair carry, and the quantity
/// the first of the divergent pair carries.
///
/// Exactly representable in a binary float, so `contract_mutants`'s
/// `Defect::QuantityThroughFloat` changes nothing this check can see. Every
/// assertion here compares a plugin's answer against a fixture this module
/// built, so a rewritten quantity would come back unequal and be reported
/// here for a mistake that is `quantity-round-trip`'s rule.
const DEDUP_CONCURRENT_QUANTITY: &str = "5.5";

/// The quantity the **divergent** racer carries instead.
///
/// The divergence has to be in a caller-supplied field that is not an
/// identity input, so the submission derives its partner's own `id` and still
/// fails [`StoredUsageRecord::caller_supplied_eq`]. The quantity is such a
/// field: it is no input to [`crate::id::derive_usage_record_id`], and DESIGN
/// §3.1's "Collision resolution" compares *"exact equality of the
/// caller-supplied fields"*, the quantity among them.
///
/// Exactly representable in a binary float for the reason
/// [`DEDUP_CONCURRENT_QUANTITY`] gives, plus one of its own: a rewrite landing
/// both quantities on one value would make the divergent racer an identical
/// one, and the conflict assertion would hold a conforming backend to an
/// outcome it must not give.
const DEDUP_CONCURRENT_DIVERGENT_QUANTITY: &str = "7.25";

/// The read limit the `Eventual` half dispatches, on the ledger page, in the
/// fold's query and on each feed page alike: twice the identities it expects.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives: **the row count is the whole of the `Eventual` half**, so a limit
/// set to the expected count would truncate the extra row away and the
/// assertion would pass against a backend holding both writes.
const DEDUP_CONCURRENT_PAGE_LIMIT: u64 = 4;

/// How many feed pages the `Eventual` half will follow before it gives up.
///
/// The feed is **followed** rather than read once, for the reason
/// `server-field-round-trip`'s own page budget gives: a short page is
/// conforming, so a single read would report a backend that pages in small
/// steps as having lost entries. The loop's real exit is the cursor standing
/// still; this budget only bounds the loop.
const DEDUP_CONCURRENT_FEED_PAGES: usize = 256;

/// The two identities this check races, and the meter and range its
/// `Eventual` half reads them back over.
struct DedupConcurrentFixtures {
    /// The meter every entry is written to, the only meter this check reads,
    /// and the whole of the subscription its feed read dispatches.
    meter: MeterRef,
    /// The range all three `Eventual` surfaces dispatch, built once.
    ///
    /// Built here rather than at each read so a range the suite cannot
    /// construct is reported once as [`HARNESS_FAULT`], rather than once per
    /// surface against the plugin.
    range: TimeRange,
    /// One racer of the identical pair.
    identical: StoredUsageRecord,
    /// The other racer of the identical pair.
    ///
    /// Built by a second call to the same builder rather than cloned from
    /// [`Self::identical`], so "the two are identical" is a fact
    /// [`DedupConcurrentFixtures::guards`] establishes about the builder. A
    /// clone would make the guard assert that a value equals itself.
    identical_again: StoredUsageRecord,
    /// One racer of the divergent pair.
    divergent: StoredUsageRecord,
    /// The other racer of the divergent pair: the same identity inputs as
    /// [`Self::divergent`], a different quantity.
    divergent_other: StoredUsageRecord,
}

/// One race's result: the identity it left on the ledger, and what it
/// reported.
///
/// Separate for the reason `dedup-floor`'s `Scenario` keeps them separate: a
/// race can seed its identity **and** report a violation, and the `Eventual`
/// half counts rows only for identities actually seeded, so a refused race is
/// reported once rather than again as a missing row.
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
/// "Plugin contract tests".)
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
/// DESIGN's row drives its pairs *"through several gateway replicas"*. This
/// check reaches the backend the only way the SPI offers: two futures over
/// one `&dyn UsageCollectorPluginV1` handle, driven together by
/// `toolkit::tokio::join!`.
///
/// That is a faithful reduction, because every rule quoted above is a
/// property of the *store's own serialisation*, and none mentions the caller:
/// a plugin cannot see how many processes its submissions came from. How much
/// the two futures interleave is the backend's to decide — a plugin writing
/// to a database suspends at its await points, one deciding inside a
/// synchronous critical section is serialised by construction, which is what
/// *"Races under one identity resolve by the store's **commit order**"* asks
/// for. This check asserts the outcome, the same question either way.
///
/// What it does not reach is a plugin's own cross-process coordination — a
/// distributed lock, a leader election, a replication wait — because nothing
/// dispatched from one process can. A backend whose serialisation holds
/// within a process and fails across one passes here, breaking the
/// obligation DESIGN puts on the plugin's *"commit or replication state"*.
///
/// # What is asserted
///
/// Two identities are raced, one per pair, each under its own idempotency
/// key so that no outcome of one can be explained by the other:
///
/// 1. **The identical pair.** Two submissions of one entry, the same in
///    every caller-supplied field, dispatched together. Both answer `Ok`
///    carrying that entry, **at either level**: identical writes have no
///    content to disagree about, so an absorb and a second acknowledgement
///    are indistinguishable. What the assertion forbids is a **refusal**,
///    which reports a conflict between a caller and itself.
/// 2. **The divergent pair, under [`DedupLevel::Linearizable`].** Two
///    submissions sharing every identity input and differing in the
///    quantity, dispatched together. Exactly one answers `Ok` and the other
///    is [`UsageCollectorPluginError::IdempotencyConflict`] — *"the bound is
///    zero, and every write is decided as it commits"* — and the conflict's
///    `existing` is the entry that was accepted, which is how the losing
///    caller learns what its key is now bound to.
/// 3. **The survivor is not displaced, under
///    [`DedupLevel::Linearizable`].** A point read after the race answers
///    the accepted entry's content. No outcome above can make this
///    assertion: a backend can hand both callers the conforming answer and
///    still write the loser's content over the winner's row, which *"no
///    later write displaces the survivor"* forbids. Nothing else in the
///    suite reads a raced identity's content back.
/// 4. **Only one write shows, on every surface, under
///    [`DedupLevel::Eventual`].** After sleeping the declared convergence
///    bound, a `list_usage_records` page holds one row per raced identity, a
///    `COUNT` fold over the same range reports one term per identity, and a
///    feed page over this check's own subscription delivers one entry per
///    identity — the same write the ledger page holds. Separate surfaces
///    because DESIGN names them separately: *"every read path, fold,
///    reconciliation figure, materialised aggregate, and the feed show it and
///    nothing else"*. A backend that converged its ledger but not its feed
///    breaks exactly that clause.
///
/// Under `Eventual` the acceptances themselves are not asserted, and the
/// quoted clause is why: *"a write can be acknowledged and then discarded"*,
/// so two acknowledgements are admissible there and only the post-bound
/// reads decide anything.
///
/// # Clauses stated here and exercised nowhere
///
/// A green run must not be read as covering these.
///
/// * **The `Transient`-then-commits clause.** DESIGN's row closes: *"At
///   either level, an insert reported `Transient` that commits after
///   convergence on a divergent retry, despite an earlier `accepted_at`,
///   changes nothing."* Asserting it needs a backend that can be told to
///   report `Transient` and commit anyway, on a schedule the check chooses.
///   The SPI has no such input, and inventing one means a defect switch
///   inside `reference`, the exemplar a plugin author copies.
/// * **The collision count.** The row's `eventual` half ends *"and the
///   divergent pair counts one collision"*. That is a published metric
///   (§3.10), and no [`UsageCollectorPluginV1`] method reports one — a
///   behavioural suite cannot read a counter.
/// * **Which write won, under `Eventual`.** DESIGN names the survivor as
///   *"the first write in commit order"*, and a conforming `eventual`
///   backend may acknowledge both, so nothing this check reads says which
///   committed first. Probe 4 therefore asserts the observable projection —
///   one write, the same one everywhere — not the first committer's
///   identity. Under `linearizable` the outcomes do name it, which is what
///   probes 2 and 3 rest on.
///
/// # Which of these assertions any subject reaches
///
/// Measured by neutering each in turn, not reasoned:
///
/// * **Probe 3 is individually load-bearing**;
///   `contract_mutants`'s `Defect::ADivergentWriteDisplacesTheSurvivor` is
///   built for it, and neutering probe 3 leaves that subject's row empty. It
///   is the only assertion in the suite that subject reaches: it answers
///   every collision the conforming way and only the stored row moves.
/// * **Probe 4's surfaces are load-bearing together and none alone.**
///   `Defect::LedgerHasNoUniqueConstraint` stores both writes of a raced
///   identity, so page, fold and feed each report; neutering any one leaves
///   the others reporting, and neutering all takes this check out of that
///   subject's row.
/// * **Probes 1 and 2 are reached by no subject**, and the gaps are the two
///   `dedup-floor` already records. Probe 1's is that no subject refuses an
///   identical re-delivery of a *record*. Probe 2's is the one
///   `Defect::LedgerHasNoUniqueConstraint`'s docs name: the subject that
///   would close it absorbs a divergent re-delivery of a record, and would
///   fail `dedup-floor`'s divergent submission too rather than isolate here.
/// * **Probe 4's cross-surface assertion is a recorded gap.** Closing it
///   needs a backend whose feed answers a different write of one identity
///   from its ledger page — the same *shape* of defect
///   `Defect::LedgerHasNoUniqueConstraint`'s docs already record as open. It
///   stays because the clause is DESIGN's own and this is the only check
///   that reads the feed for it.
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
/// The comparison is against the fixture rather than either outcome, which is
/// the point of an identical pair: whichever racer the backend made durable,
/// the entry it answers with carries this content.
///
/// **Two separate one-entry submissions, raced — not one two-entry batch.**
/// The contract under test is cross-call dedup resolution: two writers
/// colliding on one identity, resolved by the store's own commit order. One
/// batch is one writer, resolved by the SPI's intra-batch rule ("later
/// against earlier"), a different obligation. So each arm stays its own call
/// through [`seed_usage_record`](crate::contract::fixtures::seed_usage_record).
///
/// **Nothing in the suite enforces that, on this arm.** Replacing this race
/// with a single batch carrying both racers leaves the whole SDK suite green:
/// an identical pair has no content to disagree about, so both rules answer
/// it with the same entry. The same substitution in
/// [`the_divergent_pair_resolves_once`] *is* caught, by `contract_tests`'s
/// `each_check_fails_against_its_own_defect_and_no_other`. Here the comment
/// is the only guard.
async fn the_identical_pair_is_absorbed(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
) -> Race {
    let entry = &fixtures.identical;
    let (left, right) = toolkit::tokio::join!(
        seed_usage_record(plugin, &fixtures.meter, fixtures.identical.clone()),
        seed_usage_record(plugin, &fixtures.meter, fixtures.identical_again.clone()),
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
///
/// Two separate one-entry submissions, raced — not one two-entry batch, for
/// the reason [`the_identical_pair_is_absorbed`] gives.
async fn the_divergent_pair_resolves_once(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
    level: DedupLevel,
) -> Race {
    let id = fixtures.divergent.id;
    let (left, right) = toolkit::tokio::join!(
        seed_usage_record(plugin, &fixtures.meter, fixtures.divergent.clone()),
        seed_usage_record(plugin, &fixtures.meter, fixtures.divergent_other.clone()),
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
/// both callers exactly as a conforming one would and then write the loser's
/// content over the winner's row, which an `ON CONFLICT ... DO UPDATE` on the
/// dedup identity does: both callers were told the truth and the ledger holds
/// the other submission.
///
/// A point read rather than a range, so the probe is not coupled to the
/// period rule or to whatever else a range holds. The comparison is
/// [`is_the_stored`] — the id alone cannot see a displacement, since the two
/// racers derive one id by construction.
async fn the_survivor_is_not_displaced(
    plugin: &dyn UsageCollectorPluginV1,
    accepted: &StoredUsageRecord,
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
/// Each assertion is reported on its own, and each read is taken once and
/// shared: a page that could not be read is one failure rather than a premise
/// missing from several messages.
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
    items: &[StoredUsageRecord],
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
/// A second surface rather than a restatement of the first: a fold is not
/// served from the ledger page, so a backend whose `COUNT` runs against a
/// materialised aggregate can hold one row on the ledger and count both
/// writes of the race.
///
/// `COUNT` rather than `SUM` because it reads no quantity: the divergent
/// racers differ exactly in the quantity, so a `SUM` could not tell a second
/// surviving write from a differently resolved race.
async fn the_fold_counts_one_per_identity(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
    seeded: &[(&'static str, Uuid)],
) -> Vec<ContractViolation> {
    let result = match plugin
        .query_aggregated_usage_records(
            &fixtures.meter,
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
/// Its own assertion rather than a rephrasing of the ledger page's: DESIGN
/// names the feed beside the read paths and the folds, and it is the surface a
/// charging consumer reads *instead of* `list_usage_records`, so a race the
/// feed never resolved is a measurement rated twice.
fn the_feed_delivers_one_write_per_identity(
    entries: &[StoredUsageRecord],
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
/// that converged its ledger and not its feed shows one entry on each — both
/// counts right — and two different writes of the same race, where DESIGN
/// admits exactly one survivor for all surfaces.
///
/// An identity absent from either surface is passed over: the counts above
/// already report it.
fn the_feed_and_the_ledger_agree(
    items: &[StoredUsageRecord],
    entries: &[StoredUsageRecord],
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
fn rows_per_identity(rows: &[StoredUsageRecord], seeded: &[(&'static str, Uuid)]) -> Vec<String> {
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
/// the identity is seeded even though this submission was not. A race whose
/// outcomes say nothing about the store must not have its rows counted.
fn identity_reached_the_store(
    outcomes: &[&Result<StoredUsageRecord, UsageCollectorPluginError>],
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
/// ([`StoredUsageRecord::caller_supplied_eq`]) rather than either alone: the
/// two racers of an identity derive one id by construction, so the id alone
/// cannot tell the survivor from the write that lost, and the fields alone
/// would admit the right content under some other entry's id.
///
/// Whole-record equality is deliberately not used: it also reads
/// `accepted_at` and `origin`, which are server-assigned and no part of this
/// check's rule, so a backend stamping its own instant would be reported here
/// as well as by the check built for that rule.
fn is_the_stored(stored: &StoredUsageRecord, expected: &StoredUsageRecord) -> bool {
    stored.id == expected.id && stored.caller_supplied_eq(expected)
}

/// One entry's identity and the caller-supplied field this check's racers
/// differ in, rendered for a report.
///
/// The quantity rather than the whole record: the divergent racers agree on
/// everything else by construction, so it is the one field saying which a
/// surface answered with, and the id alone renders the two identically.
fn rendered(record: &StoredUsageRecord) -> String {
    format!(
        "record {id} carrying the quantity `{quantity}`",
        id = record.id,
        quantity = record.quantity,
    )
}

/// Every entry the range under test comes back with on the ledger path.
///
/// A `Vec` rather than a set, because the count is the assertion: a set would
/// collapse the second surviving write this surface exists to find. `Err`
/// carries a ready-to-report detail.
async fn dedup_concurrent_page(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
) -> Result<Vec<StoredUsageRecord>, String> {
    let page = plugin
        .list_usage_records(
            &fixtures.meter,
            fixtures.range,
            &contract_query(DEDUP_CONCURRENT_PAGE_LIMIT),
            &[],
            None,
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
/// [`check_meter`] exists for: a feed read selects by meter, so naming the
/// suite's shared meter would carry whatever other checks left on it and make
/// the observation turn on dispatch order. One meter carries both raced
/// identities, and the assertions are per identity.
///
/// The scope is [`contract_scope`], the same single-tenant compiled scope the
/// ledger path dispatches. Every entry this check stores is inside it, so the
/// read buys shape coverage and no scope *enforcement*, for the reason
/// [`SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH`](crate::contract::SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH)
/// gives.
///
/// `Err` carries a ready-to-report detail.
async fn dedup_concurrent_feed(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &DedupConcurrentFixtures,
) -> Result<Vec<StoredUsageRecord>, String> {
    let subscription = [fixtures.meter.clone()];
    let scope = contract_scope();
    let mut delivered: Vec<StoredUsageRecord> = Vec::new();
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
/// independent reasons: [`run_all`](crate::contract::run_all) dispatches
/// every check against one persistent backend that never removes an entry and
/// this check counts rows, and its feed read selects by meter.
///
/// **Each idempotency key is submitted over one covered period**, for the
/// reason `dedup-floor`'s own fixtures give: `contract_mutants`'s
/// `Defect::DedupIgnoresThePeriod` keys an index on the derived identity with
/// its period bounds struck out, so a check reusing one key across two
/// periods would appear in that subject's row for a rule that is not this
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
            &meter,
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
    /// The facts this check's assertions read, established rather than
    /// assumed. All are the suite's own rather than the plugin's, so all are
    /// reported as [`HARNESS_FAULT`].
    ///
    /// * **The identical pair is one identity.** Two ids would be two entries
    ///   colliding on nothing, every backend would accept both, and probe one
    ///   would assert nothing.
    /// * **The identical pair is genuinely identical**, in every field
    ///   [`StoredUsageRecord::caller_supplied_eq`] reads — a fact about the
    ///   builder, since the two are built by two calls rather than cloned. A
    ///   builder that varied anything would turn probe one's race into the
    ///   divergent one, where a conforming backend answers
    ///   `IdempotencyConflict`.
    /// * **The divergent pair is one identity and really diverges.** The
    ///   quantity is no identity input, so a pair differing only there is one
    ///   identity submitted twice; and a racer equal in every caller-supplied
    ///   field is an ordinary absorbed retry, where probe two requires a
    ///   conflict.
    /// * **The two identities are two.** The `Eventual` half counts one row
    ///   per identity per surface, so two races sharing an id would make
    ///   every expected count wrong and have the second race's racers resolve
    ///   against the first's.
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
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) gives:
/// in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's.
fn dedup_concurrent_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{DEDUP_CONCURRENT}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
