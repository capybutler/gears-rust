//! The DESIGN §3.3 `at-most-one-invalidation` check.
//!
//! See [`at_most_one_invalidation`] for what it asserts; the module holds
//! the sequential half, the same-batch race, and the two targets they
//! withdraw.

use rust_decimal::Decimal;

use crate::contract::fixtures::{FIXTURE_EPOCH, fixture_invalidation, fixture_record, violation};
use crate::contract::{AT_MOST_ONE_INVALIDATION, ContractViolation, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::models::{IdempotencyKey, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;

/// The start of the sequential half's covered period, and the instant this
/// check's four entries are arranged from.
///
/// A hundred and twenty days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives. This check reads nothing back, so the separation buys less here
/// than elsewhere — it keeps these entries out of the ranges the checks
/// that *do* read back dispatch.
const AT_MOST_ONE_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(120));

/// The quantity every entry of this check carries, withdrawals included: an
/// invalidation echoes the quantity it withdraws. Nothing here asserts on
/// a quantity.
const AT_MOST_ONE_QUANTITY: Decimal = Decimal::ONE;

/// The two targets this check withdraws, and the withdrawals aimed at them.
struct AtMostOneFixtures {
    /// The entry the sequential half withdraws twice.
    sequential_target: UsageRecord,
    /// The withdrawal that must be accepted.
    first_withdrawal: UsageRecord,
    /// The withdrawal that must be rejected.
    second_withdrawal: UsageRecord,
    /// The entry the concurrent half withdraws twice, in one batch.
    concurrent_target: UsageRecord,
    /// The two withdrawals racing for it. Exactly one may be accepted.
    racing_withdrawals: [UsageRecord; 2],
}

/// `at-most-one-invalidation` — *"A second withdrawal of one record is
/// rejected. Under two concurrent submissions exactly one succeeds."*
///
/// **This check is the store's, not the gateway's.** Three of the rules an
/// invalidation must satisfy against its target need a lookup and belong to
/// the ingestion gateway; this one does not, because only the store can
/// make the check atomic with the entry it admits
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`). A gateway-side
/// pre-read cannot exclude a concurrent second submission, so it would fail
/// exactly when it matters, and nothing upstream of the SPI enforces the
/// rule at all.
///
/// The sequential half asserts the rejection's **variant**, not merely that
/// the submission failed. A backend answering
/// [`UsageCollectorPluginError::Internal`] is not conforming: the gateway
/// lifts `AlreadyInvalidated` to a `409` naming the invalidation already in
/// place, and an `Internal` becomes a `500` naming nothing. An `is_err()`
/// assertion cannot tell the two apart, so it would accept a backend whose
/// callers can never learn why their withdrawal was refused. The carried
/// `invalidated_by` is asserted for the same reason: the rejection has to
/// name the entry that already withdrew the target.
///
/// The concurrent half is a **same-batch race**, driven through
/// `create_usage_records` rather than two overlapping `create_usage_record`
/// calls. The SPI aligns per-entry outcomes to input order, so two
/// withdrawals of one target in a single batch make the race expressible
/// deterministically — exactly one `Ok` and exactly one
/// `AlreadyInvalidated`, every run, with no timing to lose. A backend that
/// serialises a batch entry by entry, deciding each against the state the
/// entries before it left, satisfies this; one that evaluates the whole
/// batch against *pre-existing* state — decide once, then insert all —
/// admits both, and that is the defect being hunted. Such a backend passes
/// the sequential half, because there the two withdrawals arrive in
/// different calls and the first is already stored by the time the second
/// is decided.
pub async fn at_most_one_invalidation(
    plugin: &dyn UsageCollectorPluginV1,
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
    let mut violations = sequential_second_withdrawal(plugin, &fixtures).await;
    violations.extend(same_batch_withdrawal_race(plugin, &fixtures).await);
    violations
}

/// The sequential half: a second withdrawal of one record, in its own call,
/// is rejected as `AlreadyInvalidated` naming the invalidation in place.
async fn sequential_second_withdrawal(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &AtMostOneFixtures,
) -> Vec<ContractViolation> {
    for (role, record) in [
        ("the entry to be withdrawn", &fixtures.sequential_target),
        ("the first withdrawal of it", &fixtures.first_withdrawal),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                AT_MOST_ONE_INVALIDATION,
                format!(
                    "`create_usage_record` refused {role} (record {id}), so there was no accepted \
                     withdrawal for a second one to be rejected against: {err}",
                    id = record.id,
                ),
            )];
        }
    }

    let target = fixtures.sequential_target.id;
    let already = fixtures.first_withdrawal.id;
    let detail = match plugin
        .create_usage_record(fixtures.second_withdrawal.clone())
        .await
    {
        Err(UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by })
            if id == target && invalidated_by == already =>
        {
            return Vec::new();
        }
        Err(UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by }) => format!(
            "the second withdrawal of record {target} was rejected as `AlreadyInvalidated`, and \
             it named the target {id} withdrawn by {invalidated_by} rather than the target \
             {target} withdrawn by {already}. The variant carries those two so the gateway's \
             refusal can name the entry that already withdrew the target."
        ),
        Err(other) => format!(
            "the second withdrawal of record {target} was rejected as `{other}`, and the store's \
             one admission-time invalidation obligation is to report it as `AlreadyInvalidated`, \
             naming target {target} and the invalidation {already} already in place. Asserting \
             only that the submission failed would accept this: the gateway lifts \
             `AlreadyInvalidated` to a conflict naming the entry in place and anything else to an \
             unclassified failure naming nothing."
        ),
        Ok(stored) => format!(
            "the second withdrawal of record {target} was admitted as record {stored_id}, and \
             record {target} already carried the accepted invalidation {already}. At most one \
             invalidation per record is the store's rule and the only place it can be enforced - \
             the gateway does not pre-read for it - so a record now carries two withdrawals and a \
             fold that excludes a withdrawn pair has three entries in it.",
            stored_id = stored.id,
        ),
    };
    vec![violation(AT_MOST_ONE_INVALIDATION, detail)]
}

/// The concurrent half: two withdrawals of one target in a single batch,
/// exactly one accepted.
///
/// The batch is what makes the race deterministic. `tokio::join!` over two
/// `create_usage_record` calls would assert the same invariant against the
/// scheduler, and a check that only sometimes reaches the state it is about
/// is a check that only sometimes holds a backend to it.
///
/// A repeated run against a backend that kept the first run's entries
/// answers the same pair, which is what keeps the suite re-runnable: the
/// winner's resubmission collides on its own `id` and is re-admitted as an
/// idempotent replay — the duplicate branch is checked before the
/// at-most-one one, precisely so an emitter's retry is not read as a
/// second withdrawal — while the loser was never stored and is refused
/// again.
async fn same_batch_withdrawal_race(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &AtMostOneFixtures,
) -> Vec<ContractViolation> {
    let target = fixtures.concurrent_target.id;
    if let Err(err) = plugin
        .create_usage_record(fixtures.concurrent_target.clone())
        .await
    {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "`create_usage_record` refused the entry the racing withdrawals aim at (record \
                 {target}), so the race had no target to be decided against: {err}"
            ),
        )];
    }

    let outcomes = match plugin
        .create_usage_records(fixtures.racing_withdrawals.to_vec())
        .await
    {
        Ok(outcomes) => outcomes,
        Err(err) => {
            return vec![violation(
                AT_MOST_ONE_INVALIDATION,
                format!(
                    "`create_usage_records` failed the whole batch carrying two withdrawals of \
                     record {target}: {err}. The obligation is per entry - exactly one of the two \
                     is accepted - so refusing the batch outright decides neither."
                ),
            )];
        }
    };

    let mut admitted = Vec::new();
    let mut refused = Vec::new();
    for outcome in outcomes {
        match outcome {
            Ok(stored) => admitted.push(stored.id),
            Err(err) => refused.push(err),
        }
    }

    // Exactly one of each, and the two counts are asserted together: the
    // SPI aligns one outcome to every input entry, so any answer that is
    // not one acceptance beside one refusal is a failure to decide the
    // race.
    if admitted.len() != 1 || refused.len() != 1 {
        return vec![violation(
            AT_MOST_ONE_INVALIDATION,
            format!(
                "one `create_usage_records` batch carried two withdrawals of record {target}, and \
                 it answered {ok} acceptances ({admitted:?}) and {failed} refusals; exactly one \
                 of the two may be admitted, and the SPI aligns one outcome to every entry. Two \
                 withdrawals of one record can arrive in the same call, so a backend evaluating \
                 the whole batch against the state it held before the batch began - decide once, \
                 then insert all - admits both, while one deciding each entry against the entries \
                 already admitted ahead of it admits one. The first passes a sequential second \
                 withdrawal, where the earlier withdrawal is already stored by the time the later \
                 is decided, which is why the rule is asserted here as well.",
                ok = admitted.len(),
                failed = refused.len(),
            ),
        )];
    }

    let mut violations = Vec::new();
    for err in &refused {
        let named = matches!(
            err,
            UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by }
                if *id == target && admitted.contains(invalidated_by)
        );
        if !named {
            violations.push(violation(
                AT_MOST_ONE_INVALIDATION,
                format!(
                    "the withdrawal of record {target} the batch turned down was refused as \
                     `{err}`, and the one refusal a losing racer earns is `AlreadyInvalidated` \
                     naming target {target} and the withdrawal the batch admitted ({admitted:?}). \
                     Losing a race and being unstorable are different outcomes, and only the \
                     first is this rule."
                ),
            ));
        }
    }
    violations
}

/// Builds both targets and the three withdrawals aimed at them.
///
/// The guard is that the two racing withdrawals derive **different** ids.
/// If they did not, the second would collide on the first's id and be
/// re-admitted as an idempotent replay of it — the duplicate-entry branch,
/// which runs before the at-most-one branch precisely so an emitter's retry
/// is not reported as a second withdrawal. The batch would then answer two
/// `Ok`s against a conforming backend, and the check would report the
/// suite's own fixture as a defect.
fn at_most_one_fixtures() -> Result<AtMostOneFixtures, String> {
    let sequential_end = AT_MOST_ONE_WINDOW_FROM.saturating_add(time::Duration::hours(1));
    let concurrent_end = AT_MOST_ONE_WINDOW_FROM.saturating_add(time::Duration::hours(2));

    let sequential_target = fixture_record(
        &at_most_one_key("sequential-target")?,
        AT_MOST_ONE_QUANTITY,
        AT_MOST_ONE_WINDOW_FROM,
        sequential_end,
    )?;
    let concurrent_target = fixture_record(
        &at_most_one_key("concurrent-target")?,
        AT_MOST_ONE_QUANTITY,
        sequential_end,
        concurrent_end,
    )?;

    // Every withdrawal carries its target's covered period and quantity,
    // which is the shape the gateway admits. Only the idempotency key
    // separates two withdrawals of one target, and it has to: the target
    // reference is deliberately not an input to the derived identity, so
    // two withdrawals sharing a key would be one entry.
    let withdrawal = |role: &str, target: &UsageRecord| -> Result<UsageRecord, String> {
        fixture_invalidation(
            &at_most_one_key(role)?,
            AT_MOST_ONE_QUANTITY,
            target.window_start,
            target.window_end,
            target.id,
        )
    };

    let first_withdrawal = withdrawal("sequential-first", &sequential_target)?;
    let second_withdrawal = withdrawal("sequential-second", &sequential_target)?;
    let racing_first = withdrawal("concurrent-first", &concurrent_target)?;
    let racing_second = withdrawal("concurrent-second", &concurrent_target)?;

    if racing_first.id == racing_second.id {
        return Err(format!(
            "the two withdrawals racing for record {target} derive one id ({id}), so the second \
             would be an idempotent replay of the first rather than a second withdrawal, and a \
             conforming backend would answer two acceptances",
            target = concurrent_target.id,
            id = racing_first.id,
        ));
    }
    if first_withdrawal.id == second_withdrawal.id {
        return Err(format!(
            "the two sequential withdrawals of record {target} derive one id ({id}), so the \
             second would be an idempotent replay of the first and a conforming backend would \
             accept it",
            target = sequential_target.id,
            id = first_withdrawal.id,
        ));
    }

    Ok(AtMostOneFixtures {
        sequential_target,
        first_withdrawal,
        second_withdrawal,
        concurrent_target,
        racing_withdrawals: [racing_first, racing_second],
    })
}

/// The idempotency key one role of this check's fixture submits under,
/// keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) gives.
fn at_most_one_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{AT_MOST_ONE_INVALIDATION}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
