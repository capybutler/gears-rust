//! The DESIGN §3.3 `at-most-one-invalidation` check.
//!
//! See [`at_most_one_invalidation`] for what it asserts.

use bigdecimal::BigDecimal;

use crate::contract::fixtures::{
    FIXTURE_EPOCH, contract_query, fixture_invalidation_with_reason, fixture_record, violation,
};
use crate::contract::{AT_MOST_ONE_INVALIDATION, ContractViolation, DedupLevel, HARNESS_FAULT};
use crate::error::UsageCollectorPluginError;
use crate::models::{AggregationFold, IdempotencyKey, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The start of this check's covered periods: a hundred and twenty days past
/// [`FIXTURE_EPOCH`], clear of the ranges the checks that read back dispatch.
const AT_MOST_ONE_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(120));

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

/// The pair decided across separate calls, and the pair decided in one batch.
struct AtMostOneFixtures {
    separate: Pair,
    batched: Pair,
}

/// `at-most-one-invalidation` — *"At the SPI, a second invalidation of one
/// record under the same reason code returns the stored invalidation, and under
/// a different one is `IdempotencyConflict`. Concurrent submissions follow the
/// declared dedup level."*
///
/// There is no store-side at-most-one rule to test. Every invalidation of one
/// record repeats that record's idempotency key and carries
/// `entry_type = invalidation`, so all of them agree on the six identity
/// inputs and share one identity — and none shares the record's. A second one
/// is therefore an ordinary collision on the dedup identity, and this check
/// holds a plugin to the dedup outcomes on it:
///
/// * **Separate calls.** Resubmitting the accepted withdrawal answers with the
///   stored invalidation; submitting one under another reason code is
///   `IdempotencyConflict` whose `existing` is the accepted withdrawal.
/// * **One batch call.** Of two withdrawals of one record in a single
///   `create_usage_records`, the first is accepted and the second resolves
///   against it as `IdempotencyConflict`.
///
/// Under [`DedupLevel::Eventual`] a divergent withdrawal may be acknowledged
/// and later discarded, so an acceptance is not a violation there. After the
/// declared convergence bound the ledger must hold exactly one invalidation of
/// each record, and a `COUNT` over the pair's range must count nothing.
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
    if let DedupLevel::Eventual { convergence_bound } = level {
        toolkit::tokio::time::sleep(convergence_bound).await;
        for pair in [&fixtures.separate, &fixtures.batched] {
            violations.extend(one_invalidation_survives(plugin, pair).await);
        }
    }
    violations
}

/// Whether `existing` is the accepted withdrawal of `pair`, reason code included.
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
        (Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }), _)
            if names_the_first(&existing, pair) => {}
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
        (Some(Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. })), _)
            if names_the_first(&existing, pair) => {}
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
    Ok(AtMostOneFixtures {
        separate: pair(
            "separate-target",
            quantity,
            AT_MOST_ONE_WINDOW_FROM,
            separate_end,
        )?,
        batched: pair("batched-target", quantity, separate_end, batched_end)?,
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
