//! The DESIGN §3.3 `at-most-one-invalidation` check.
//!
//! See [`at_most_one_invalidation`] for what it asserts.

use bigdecimal::BigDecimal;

use crate::contract::fixtures::{
    check_window_from, contract_query, fixture_invalidation_with_reason, fixture_record, violation,
};
use crate::contract::{AT_MOST_ONE_INVALIDATION, ContractViolation, DedupLevel, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::models::{AggregationFold, IdempotencyKey, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The start of this check's covered periods: the offset
/// [`check_window_from`] tables for this check, clear of the ranges the
/// checks that read back dispatch.
///
/// **The day offset is this check's whole separation from the others, and
/// the three pairs below separate from each other inside it.** Every fixture
/// here is built by [`fixture_record`], which puts it on the one shared
/// meter, so nothing but the covered period keeps one check's entries out of
/// another's reads. The pairs take successive hours inside this check's own
/// day, and each pair's post-convergence range is the minute after its own
/// period end, so no range of one pair selects an entry of another. A pair
/// added here takes the next hour rather than a row of its own in the
/// table: a row there is what one check separates from another by, and a
/// third pair of this check is not a third check.
const AT_MOST_ONE_WINDOW_FROM: time::OffsetDateTime =
    check_window_from(AT_MOST_ONE_INVALIDATION, "main");

/// The quantity every entry here carries; an invalidation echoes it.
const AT_MOST_ONE_QUANTITY: &str = "1";

/// The reason code the accepted withdrawal of each target states.
const FIRST_REASON: &str = "at-most-one-invalidation-first";

/// The reason code the divergent withdrawal of each target states.
const SECOND_REASON: &str = "at-most-one-invalidation-second";

/// The read limit of the `Eventual` ledger read: twice the entries one pair's
/// range holds.
const AT_MOST_ONE_PAGE_LIMIT: u64 = 8;

/// A record and two withdrawals of it that differ in their reason code alone.
struct Pair {
    target: UsageRecord,
    first: UsageRecord,
    second: UsageRecord,
}

/// The pair decided across separate calls, the pair decided in one batch, and
/// the pair whose two withdrawals are dispatched together.
struct AtMostOneFixtures {
    separate: Pair,
    batched: Pair,
    concurrent: Pair,
}

/// `at-most-one-invalidation` — *"At the SPI, a second invalidation of one
/// record under the same reason code returns the stored invalidation, and
/// under a different one is `IdempotencyConflict` whose `existing` is that
/// invalidation, never the record. Concurrent submissions follow the declared
/// dedup level."* (DESIGN §3.3, "Plugin contract tests", line 1314.)
///
/// There is no store-side at-most-one rule to test. Every invalidation of one
/// record repeats that record's idempotency key and carries
/// `entry_type = invalidation`, so all of them agree on the six identity
/// inputs and share one identity — and none shares the record's. A second one
/// is therefore an ordinary collision on the dedup identity, and this check
/// holds a plugin to the dedup outcomes on it, over three targets:
///
/// * **Separate calls.** Resubmitting the accepted withdrawal answers with the
///   stored invalidation; submitting one under another reason code is
///   `IdempotencyConflict` whose `existing` is the accepted withdrawal.
/// * **One batch call.** Of two withdrawals of one record in a single
///   `create_usage_records`, the first is accepted and the second resolves
///   against it as `IdempotencyConflict`.
/// * **Together, over one handle.** Two withdrawals of a third record,
///   differing in their reason code alone, dispatched by
///   `toolkit::tokio::join!`. Under [`DedupLevel::Linearizable`] exactly one
///   is accepted and the other is `IdempotencyConflict` naming it; under
///   [`DedupLevel::Eventual`] neither outcome is asserted and the
///   post-convergence read below is the whole of what that declaration is
///   held to.
///
/// Under [`DedupLevel::Eventual`] a divergent withdrawal may be acknowledged
/// and later discarded, so an acceptance is not a violation there. After the
/// declared convergence bound the ledger must hold exactly one invalidation of
/// each of the three records, and a `COUNT` over each pair's range must count
/// nothing.
///
/// # `existing` is the withdrawal, never the record — two claims, not one
///
/// The row's clause *"whose `existing` is that invalidation, never the
/// record"* rules out **two** different answers, and this check reports them
/// separately:
/// [`is_an_invalidation_of`] asks whether the entry handed back withdraws the
/// target at all, and [`names_the_first`] asks whether it is the withdrawal
/// the store actually admitted. A backend answering the *record* and a
/// backend answering the *wrong withdrawal* are making different mistakes,
/// and folding the two into one bool hands them one diagnostic.
///
/// **The first claim is not individually load-bearing, and it is kept for the
/// report it produces rather than for a subject it alone catches.** That was
/// measured: neutering it leaves `contract_mutants`'s
/// `Defect::ConflictNamesTheRecord` reported by the second instead — the
/// target record carries neither the accepted withdrawal's id nor its reason
/// code, so the comparison is false of it — and no test in the suite fails.
/// What the split buys is that the subject is caught by the assertion whose
/// prose describes its mistake, rather than told it named the wrong
/// withdrawal when it named no withdrawal at all.
///
/// # The concurrent half, and what a task stands in for
///
/// Two futures over one `&dyn UsageCollectorPluginV1` handle, driven together
/// by `toolkit::tokio::join!`. The argument that this is a faithful reduction
/// of DESIGN's *"several gateway replicas"* — and what it does not buy — is
/// [`dedup_concurrent`](super::dedup_concurrent())'s, stated there in full
/// and not restated here: the reduction is the same one, over the same SPI,
/// for the same reason. What is different is only *what* is raced. That check
/// races two **records**; this one races two **withdrawals of one record**,
/// which is the identity DESIGN's row is about and which no other check
/// submits concurrently.
///
/// Under `Linearizable` the half asserts three things, and the middle one is
/// there to keep the last from being satisfiable by agreement alone:
///
/// 1. The pair resolves into exactly one `Ok` and one `IdempotencyConflict`.
/// 2. The accepted entry is one of the two withdrawals **as this module built
///    them**. A backend could otherwise answer both callers with content of
///    its own invention and satisfy the next assertion by answering itself.
/// 3. The conflict's `existing` is that accepted entry, which is how the
///    losing caller learns what its key is now bound to.
///
/// # Which of these assertions any subject reaches
///
/// Measured by neutering each in turn against the whole subject list, not
/// reasoned. Three assertions are individually load-bearing and the rest are
/// not, which is a statement about the subjects rather than about the rule:
///
/// * **The exact-retry absorb is load-bearing.**
///   `Defect::RefusesAWithdrawalWithTheSameReason` reaches this assertion and
///   no other in the suite, and neutering it empties that subject's row.
/// * **The one-batch shape is load-bearing.**
///   `Defect::BatchResolvesAgainstThePreCallLedger` reaches it and nothing
///   else here; neutering it takes this check out of that subject's row,
///   which then names `dedup-floor` alone.
/// * **The post-convergence survivor count is load-bearing.** It is what puts
///   this check in `Defect::LedgerHasNoUniqueConstraint`'s `Eventual` column;
///   the `COUNT` beside it does not, because a pair with two withdrawals on
///   the ledger is still a withdrawn pair and still counts nothing.
/// * **Everything the two new halves added is matrix-redundant, and each is
///   kept for a stated reason.** `Defect::ConflictNamesTheRecord` is reported
///   three times over — by the split claim on each of the two sequential call
///   shapes and by the concurrent half's third assertion — so no one of them
///   empties its row. The split claim is kept for its diagnostic, above. The
///   concurrent half is kept because it is the only place in this suite where
///   **two withdrawals of one record** are dispatched together, which is the
///   identity DESIGN's row is about; `dedup-concurrent` races records.
/// * **No subject can isolate the concurrent half, and that is a property of
///   the subjects.** A subject wrong about concurrent resolution and right
///   about the sequential one would have to decide a race differently from
///   the way it decides a sequence, and a mutant that loses a race
///   deterministically is a mutant that is not racing. Both subjects that
///   reach the half — `Defect::AbsorbsAWithdrawalWithAnotherReason` and
///   `Defect::DedupIgnoresTheEntryType` — are wrong about the sequential
///   halves too, and are reported there as well. What the half was shown to
///   catch was established by hand instead: the exemplar's own `admit`,
///   altered to absorb a divergent collision on an invalidation, is reported
///   by this half and by the two sequential shapes alike.
///
/// # Assertions no subject reaches
///
/// Each was inverted to establish it is on a live path rather than dead
/// code — inverted, the reference backend reports it — and each is recorded
/// rather than deleted:
///
/// * **A retry answered with something other than the stored invalidation.**
///   The subject that would close it answers an exact retry with a different
///   entry. None does: the one subject that re-decides a collision against a
///   row of its own choosing,
///   `Defect::ConflictReadBackIgnoresTheEntryType`, picks the **last** row
///   under the five components, and a withdrawal is always the later arrival
///   of a record and its withdrawal, so it picks the right one here. A
///   pre-existing gap, unchanged by the concurrent half.
/// * **`existing` is an invalidation of this target but not the accepted
///   one.** Every subject that answers a conflict with the wrong entry
///   answers it with the *record*, so the claim above it decides them all
///   and this one is the arm nothing reaches. The subject that would close it
///   hands back the divergent withdrawal — the content it refused — and
///   would fail `dedup-floor`'s divergent submission for the same reason,
///   so it would widen that check's row rather than isolate here.
/// * **The acceptance is one of the two submissions.** It exists so that the
///   assertion after it cannot be satisfied by a backend answering both
///   callers with content of its own invention: that comparison is between
///   two things the plugin produced, and this one is against a fixture this
///   module built. No subject fabricates content, so none reaches it.
/// * **The four precondition reports** — the batch call's target, the batch
///   call itself, its arity, and the raced pair's target. A subject built to
///   fail one of them refuses an ordinary submission and fails most of the
///   suite, so none exists. The separate-call pair's own precondition report
///   is the exception and *is* reached, by
///   `Defect::DedupIgnoresTheEntryType`, whose index collides the first
///   withdrawal with the record it withdraws.
/// * **The two read-failure reports and the two harness faults.** Each is the
///   other arm of a match whose success arm is demonstrably live, so nothing
///   is dead; they report a backend or a suite that broke before the rule
///   could be asked.
pub async fn at_most_one_invalidation(
    plugin: &dyn UsageCollectorPluginV1,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let fixtures = match at_most_one_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{AT_MOST_ONE_INVALIDATION}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in the \
                     plugin under test: {detail}"
                ),
            )];
        }
    };
    let mut violations = separate_calls(plugin, &fixtures.separate, level).await;
    violations.extend(one_batch_call(plugin, &fixtures.batched, level).await);
    violations.extend(concurrent_calls(plugin, &fixtures.concurrent, level).await);
    if let DedupLevel::Eventual { convergence_bound } = level {
        toolkit::tokio::time::sleep(convergence_bound).await;
        for pair in [&fixtures.separate, &fixtures.batched, &fixtures.concurrent] {
            violations.extend(one_invalidation_survives(plugin, pair).await);
        }
    }
    violations
}

/// Whether `existing` is the accepted withdrawal rather than its target.
///
/// Separate from the reason-code comparison deliberately. A backend whose
/// conflict names the **record** is making a different mistake from one whose
/// conflict names the wrong withdrawal, and folding both into one bool gives
/// them one diagnostic. DESIGN §3.3 calls the first out by name: the
/// `existing` is "that invalidation, never the record".
fn is_an_invalidation_of(existing: &UsageRecord, pair: &Pair) -> bool {
    existing
        .invalidation
        .as_ref()
        .is_some_and(|invalidation| invalidation.target == pair.target.id)
}

/// Whether `existing` is the accepted withdrawal of `pair`, reason code included.
///
/// Asked only of an entry [`is_an_invalidation_of`] has already accepted, so
/// this compares two withdrawals of one target and never a withdrawal against
/// the measurement. That ordering is what keeps the two diagnostics apart:
/// the id comparison below is false of the target record too, so an unguarded
/// call would report a backend that named the record for naming the wrong
/// withdrawal.
fn names_the_first(existing: &UsageRecord, pair: &Pair) -> bool {
    // Both withdrawals repeat the target's key under `entry_type =
    // invalidation`, so they agree on all six identity inputs and the fixture
    // already makes the ids equal; the reason code is what tells the accepted
    // one from the divergent one.
    existing.id == pair.first.id
        && existing
            .invalidation
            .as_ref()
            .map(|invalidation| invalidation.reason.as_str())
            == Some(FIRST_REASON)
}

/// Whether `stored` is `expected` as the plugin would answer with it.
fn is_the_stored(stored: &UsageRecord, expected: &UsageRecord) -> bool {
    stored.id == expected.id && stored.caller_supplied_eq(expected)
}

/// The two claims DESIGN §3.3 makes about the `existing` a divergent
/// withdrawal is refused with, reported one at a time.
///
/// `dispatch` names the submission that was refused, so one helper serves
/// every call shape this check drives and each still says which of them the
/// backend answered this way.
fn the_conflict_names_the_accepted_withdrawal(
    existing: &UsageRecord,
    pair: &Pair,
    dispatch: &str,
) -> Vec<ContractViolation> {
    let target = pair.target.id;
    let first = pair.first.id;
    if !is_an_invalidation_of(existing, pair) {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "{dispatch} was refused as `IdempotencyConflict`, and the `existing` it carried is \
                 {existing:?} - an entry that withdraws no record {target}. The colliding identity \
                 is the withdrawal's own: both withdrawals repeat record {target}'s idempotency \
                 key under `entry_type = invalidation`, so both derive {first} and neither derives \
                 {target}. The entry handed back is therefore the stored withdrawal, and DESIGN \
                 names the answer this rules out - `existing` is that invalidation, never the \
                 record. Handing back the target tells a caller its withdrawal collided with the \
                 measurement, which is a collision the store never decided."
            ),
        )];
    }
    if names_the_first(existing, pair) {
        return Vec::new();
    }
    vec![violation(
        AT_MOST_ONE_INVALIDATION,
        format!(
            "{dispatch} was refused as `IdempotencyConflict` carrying {existing:?}. That entry \
             does withdraw record {target}, and it is not the withdrawal the store admitted: the \
             accepted one is {first} under reason code `{FIRST_REASON}`. A conflict's `existing` \
             is what the colliding key is now bound to, so naming another withdrawal of the same \
             target tells the losing caller its key resolved to content the store never kept."
        ),
    )]
}

async fn separate_calls(
    plugin: &dyn UsageCollectorPluginV1,
    pair: &Pair,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    for (role, record) in [
        ("the entry to be withdrawn", &pair.target),
        ("the first withdrawal of it", &pair.first),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                AT_MOST_ONE_INVALIDATION,
                format!(
                    "`create_usage_record` refused {role} (record {id}), so there was no accepted \
                     withdrawal to decide a second one against: {err}",
                    id = record.id,
                ),
            )];
        }
    }
    let target = pair.target.id;
    let first = pair.first.id;
    let mut violations = Vec::new();

    match plugin.create_usage_record(pair.first.clone()).await {
        Ok(stored) if is_the_stored(&stored, &pair.first) => {}
        Ok(stored) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "resubmitting withdrawal {first} of record {target} under its own reason code \
                 answered {stored:?}; an exact retry must return the stored invalidation"
            ),
        )),
        Err(err) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "resubmitting withdrawal {first} of record {target} under its own reason code was \
                 refused as `{err}`; it is an exact retry and must be absorbed, returning the \
                 stored invalidation"
            ),
        )),
    }

    match (plugin.create_usage_record(pair.second.clone()).await, level) {
        (Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }), _) => violations
            .extend(the_conflict_names_the_accepted_withdrawal(
                &existing,
                pair,
                &format!("a second withdrawal of record {target} under another reason code"),
            )),
        (Ok(_), DedupLevel::Eventual { .. }) => {}
        (outcome, _) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "a second withdrawal of record {target} under another reason code answered \
                 {outcome:?}; it repeats that record's idempotency key under `entry_type = \
                 invalidation`, so it derives the accepted withdrawal's own id {first} and must \
                 be `IdempotencyConflict` whose `existing` is that withdrawal"
            ),
        )),
    }
    violations
}

async fn one_batch_call(
    plugin: &dyn UsageCollectorPluginV1,
    pair: &Pair,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let target = pair.target.id;
    if let Err(err) = plugin.create_usage_record(pair.target.clone()).await {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "`create_usage_record` refused the entry the batched withdrawals aim at (record \
                 {target}): {err}"
            ),
        )];
    }
    let outcomes = match plugin
        .create_usage_records(vec![pair.first.clone(), pair.second.clone()])
        .await
    {
        Ok(outcomes) => outcomes,
        Err(err) => {
            return vec![violation(
                AT_MOST_ONE_INVALIDATION,
                format!(
                    "`create_usage_records` failed the whole batch carrying two withdrawals of \
                     record {target}: {err}. Outcomes are per entry."
                ),
            )];
        }
    };
    if outcomes.len() != 2 {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "`create_usage_records` answered {} outcomes for a batch of two entries",
                outcomes.len()
            ),
        )];
    }
    let mut outcomes = outcomes.into_iter();
    let (earlier, later) = (outcomes.next(), outcomes.next());
    let mut violations = Vec::new();
    match earlier {
        Some(Ok(stored)) if is_the_stored(&stored, &pair.first) => {}
        other => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "the first of two withdrawals of record {target} in one batch answered {other:?}; \
                 it is the first entry of its identity in the call and must be accepted"
            ),
        )),
    }
    match (later, level) {
        (Some(Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. })), _) => {
            violations.extend(the_conflict_names_the_accepted_withdrawal(
                &existing,
                pair,
                &format!(
                    "the second of two withdrawals of record {target} in one batch, under another \
                     reason code"
                ),
            ));
        }
        (Some(Ok(_)), DedupLevel::Eventual { .. }) => {}
        (other, _) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "the second of two withdrawals of record {target} in one batch, under another \
                 reason code, answered {other:?}; a later same-identity entry resolves against \
                 the earlier one and must be `IdempotencyConflict` naming {first}",
                first = pair.first.id,
            ),
        )),
    }
    violations
}

/// The concurrent half: two withdrawals of one record, differing in their
/// reason code alone, dispatched together over one handle.
///
/// The target is submitted and acknowledged first, so the race is between the
/// two withdrawals and not between a withdrawal and the entry it withdraws.
///
/// Under [`DedupLevel::Eventual`] nothing here is asserted: *"a write can be
/// acknowledged and then discarded"*, so two acknowledgements are admissible
/// and no outcome names the survivor. What that declaration is held to is
/// [`one_invalidation_survives`] over this pair, which the caller runs after
/// sleeping the declared bound.
async fn concurrent_calls(
    plugin: &dyn UsageCollectorPluginV1,
    pair: &Pair,
    level: DedupLevel,
) -> Vec<ContractViolation> {
    let target = pair.target.id;
    if let Err(err) = plugin.create_usage_record(pair.target.clone()).await {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "`create_usage_record` refused the entry the raced withdrawals aim at (record \
                 {target}), so there was nothing for them to withdraw: {err}"
            ),
        )];
    }
    let (left, right) = toolkit::tokio::join!(
        plugin.create_usage_record(pair.first.clone()),
        plugin.create_usage_record(pair.second.clone()),
    );
    if !matches!(level, DedupLevel::Linearizable) {
        return Vec::new();
    }

    let ((Ok(accepted), Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }))
    | (Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }), Ok(accepted))) =
        (&left, &right)
    else {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "two withdrawals of record {target}, differing in their reason code alone, were \
                 dispatched together against one identity under a `linearizable` declaration, \
                 and they answered {left:?} and {right:?}. Both repeat that record's idempotency \
                 key under `entry_type = invalidation`, so both derive the one withdrawal \
                 identity {first}, and that declaration is a zero convergence bound with every \
                 write decided as it commits: the race resolves into one acceptance and one \
                 `IdempotencyConflict`. Two acceptances are two callers each told their reason \
                 code was recorded, of which at most one can be true; two conflicts leave the \
                 record withdrawn by neither, or by one whose caller was told it was refused.",
                first = pair.first.id,
            ),
        )];
    };

    let mut violations = Vec::new();
    if !is_the_stored(accepted, &pair.first) && !is_the_stored(accepted, &pair.second) {
        violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "the raced withdrawals of record {target} resolved into one acceptance and one \
                 `IdempotencyConflict`, and the acceptance answered {accepted:?} - which is \
                 neither of the two withdrawals that were submitted. Whichever of them the store \
                 made durable, the entry it answers with carries that submission's own \
                 caller-supplied content, reason code included."
            ),
        ));
    }
    if !is_the_stored(existing, accepted) {
        violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "the raced withdrawals of record {target} resolved into one acceptance and one \
                 `IdempotencyConflict`, and the conflict named {existing:?} where the acceptance \
                 answered {accepted:?}. The `existing` a conflict carries is the withdrawal that \
                 won the identity, which is how the losing caller learns which reason code the \
                 record is now withdrawn under; naming anything else hands it an entry no \
                 submission of this race produced, or the very content the store refused."
            ),
        ));
    }
    violations
}

/// The `Eventual` half: after the convergence bound, one invalidation of the
/// record survives on the ledger and the fold counts the pair as nothing.
async fn one_invalidation_survives(
    plugin: &dyn UsageCollectorPluginV1,
    pair: &Pair,
) -> Vec<ContractViolation> {
    let target = pair.target.id;
    let range = match TimeRange::new(
        pair.target.window_end,
        pair.target
            .window_end
            .saturating_add(time::Duration::minutes(1)),
    ) {
        Ok(range) => range,
        Err(err) => {
            return vec![violation(
                HARNESS_FAULT,
                format!("the check's own range is invalid: {err:?}"),
            )];
        }
    };
    let query = contract_query(AT_MOST_ONE_PAGE_LIMIT);
    let mut violations = Vec::new();

    match plugin
        .list_usage_records(pair.target.gts_type_id.clone(), range, &query, &[])
        .await
    {
        Ok(page) => {
            let withdrawals = page
                .items
                .iter()
                .filter(|entry| {
                    entry
                        .invalidation
                        .as_ref()
                        .is_some_and(|invalidation| invalidation.target == target)
                })
                .count();
            if withdrawals != 1 {
                violations.push(violation(
                    AT_MOST_ONE_INVALIDATION,
                    format!(
                        "after the declared convergence bound the ledger holds {withdrawals} \
                         invalidations of record {target}; one identity reads at most once, so \
                         exactly one survives"
                    ),
                ));
            }
        }
        Err(err) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!("the ledger read over record {target}'s period failed: {err}"),
        )),
    }

    match plugin
        .query_aggregated_usage_records(
            pair.target.gts_type_id.clone(),
            range,
            AggregationFold::Count,
            &query,
            &[],
            &[],
        )
        .await
    {
        Ok(result) => {
            let zero = BigDecimal::from(0);
            if result
                .buckets
                .iter()
                .filter_map(|bucket| bucket.value.as_ref())
                .any(|value| *value != zero)
            {
                violations.push(violation(
                    AT_MOST_ONE_INVALIDATION,
                    format!(
                        "a COUNT over record {target}'s withdrawn pair counted something \
                         ({:?}); a withdrawn pair counts none",
                        result.buckets
                    ),
                ));
            }
        }
        Err(err) => violations.push(violation(
            AT_MOST_ONE_INVALIDATION,
            format!("the COUNT over record {target}'s period failed: {err}"),
        )),
    }
    violations
}

fn at_most_one_fixtures() -> Result<AtMostOneFixtures, String> {
    let quantity = UsageQuantity::parse(AT_MOST_ONE_QUANTITY)
        .map_err(|err| format!("the check's own quantity literal is invalid: {err}"))?;
    let separate_end = AT_MOST_ONE_WINDOW_FROM.saturating_add(time::Duration::hours(1));
    let batched_end = AT_MOST_ONE_WINDOW_FROM.saturating_add(time::Duration::hours(2));
    let concurrent_end = AT_MOST_ONE_WINDOW_FROM.saturating_add(time::Duration::hours(3));
    Ok(AtMostOneFixtures {
        separate: pair(
            "separate-target",
            quantity,
            AT_MOST_ONE_WINDOW_FROM,
            separate_end,
        )?,
        batched: pair("batched-target", quantity, separate_end, batched_end)?,
        concurrent: pair("concurrent-target", quantity, batched_end, concurrent_end)?,
    })
}

fn pair(
    role: &str,
    quantity: UsageQuantity,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<Pair, String> {
    let key = IdempotencyKey::new(format!("{AT_MOST_ONE_INVALIDATION}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
    let target = fixture_record(&key, quantity, window_start, window_end)?;
    let first = fixture_invalidation_with_reason(&target, FIRST_REASON)?;
    let second = fixture_invalidation_with_reason(&target, SECOND_REASON)?;
    if first.id != second.id {
        return Err(format!(
            "the two withdrawals of record {} derive different ids ({} vs {}); every withdrawal \
             of one target repeats that target's idempotency key under `entry_type = \
             invalidation`, so this pair would not collide",
            target.id, first.id, second.id
        ));
    }
    Ok(Pair {
        target,
        first,
        second,
    })
}
