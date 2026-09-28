//! The DESIGN §3.3 `latest-tie-break` check.
//!
//! See [`latest_tie_break`] for what it asserts; the module holds the three
//! scenarios DESIGN §3.1's order is read from, the search that puts each
//! scenario's winner under the smaller `id`, and the single ungrouped fold
//! that decides each of them.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use toolkit_odata::ast;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, check_meter, check_window_from,
    contract_query_with_scope, contract_scope, contract_tenant, fixture_record_on,
    inverted_id_pair, violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, LATEST_TIE_BREAK};
use crate::models::{AggregationFold, IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

/// The instant every covered period of this check is offset from.
///
/// The offset [`check_window_from`] tables for this check, for the reason
/// that accessor gives.
///
/// It matters here because each of this check's three scenarios folds over a
/// range of its own and compares the answer against **one** entry's quantity.
/// A stray entry inside one of those ranges is a fourth candidate the fold
/// may legitimately pick, so it would be reported as a tie-break this backend
/// got wrong.
///
/// The offset is the second of two separations rather than the only one: the
/// scenarios are also written to meters of this check's own (see
/// [`tie_break_scenarios`]), and a range and a meter no other check writes to
/// are independent reasons why nothing else can reach a fold this check
/// dispatches.
const LATEST_TIE_FROM: time::OffsetDateTime = check_window_from(LATEST_TIE_BREAK, "main");

/// The acceptance instant the **loser** of the `window_end` scenario carries,
/// and the **winner** of the `accepted_at` scenario.
///
/// An hour past [`CONTRACT_ACCEPTED_AT`], which every other entry of this
/// check keeps. One offset serves both scenarios because each reads it in the
/// direction its own key demands: the `window_end` scenario needs its winner
/// to carry the *smaller* acceptance instant, so the loser takes this one;
/// the `accepted_at` scenario needs its winner to carry the *greater*, so the
/// winner takes it.
const LATEST_TIE_LATER_ACCEPTED_AT: time::OffsetDateTime =
    CONTRACT_ACCEPTED_AT.saturating_add(time::Duration::hours(1));

/// The quantity the `window_end` scenario's winner carries, and the whole of
/// what that fold must report.
///
/// Exactly representable in a binary float, which keeps the one subject that
/// rewrites a quantity on the way in - `contract_mutants`'s
/// `Defect::QuantityThroughFloat` - from changing what any fold here answers
/// and so from reaching this check for a rule that belongs to
/// `quantity-round-trip`. Every quantity below is chosen the same way, and
/// each pair is distinct, which is what makes a wrongly picked winner a
/// different number rather than the same one.
const WINDOW_END_WINNER_QUANTITY: &str = "6";
/// The quantity its loser carries: the entry whose period ends earlier, and
/// what a fold that never reads `window_end` reports instead.
const WINDOW_END_LOSER_QUANTITY: &str = "3";

/// The quantity the `accepted_at` scenario's winner carries: the entry of the
/// two accepted later, and what DESIGN's middle key selects.
const ACCEPTED_AT_WINNER_QUANTITY: &str = "22";
/// The quantity its loser carries, and what a fold that falls from
/// `window_end` straight through to `id` reports instead.
const ACCEPTED_AT_LOSER_QUANTITY: &str = "11";

/// The quantity the cross-tenant scenario's greater-`id` entry carries, and
/// what DESIGN's last key selects.
const ACROSS_TENANTS_WINNER_QUANTITY: &str = "88";
/// The quantity its smaller-`id` entry carries, and what a fold whose order
/// runs out after `accepted_at` reports instead.
const ACROSS_TENANTS_LOSER_QUANTITY: &str = "44";

/// The `index` this check mints its two extra tenants at.
///
/// The cross-tenant scenario needs a group spanning two tenants, and the four
/// named ids in [`super::super::fixtures`] already carry other checks'
/// entries under premises of their own -
/// [`SCOPE_UNUSED_TENANT_ID`](crate::contract::fixtures::SCOPE_UNUSED_TENANT_ID)'s
/// is that it owns none at all. [`contract_tenant`] mints from a block held
/// clear of all four, and this check is its first caller.
const ACROSS_TENANTS_FIRST_TENANT: u32 = 0;
/// The second of the two, one past the first.
const ACROSS_TENANTS_SECOND_TENANT: u32 = ACROSS_TENANTS_FIRST_TENANT + 1;

/// The bucket limit every fold here dispatches.
///
/// Each read is ungrouped, which the aggregate surface fixes as exactly one
/// bucket carrying an empty key, so the limit bounds nothing this check
/// expects. The margin is there so a backend emitting a second bucket is
/// *visible* rather than truncated away: [`ungrouped_latest`] reports any
/// bucket count other than one rather than picking a value out of it.
const LATEST_TIE_BUCKET_LIMIT: u64 = 4;

/// One scenario: the entries it stores, the fold it dispatches, and the
/// quantity DESIGN's order obliges that fold to report.
struct TieBreakScenario {
    /// Which of DESIGN's three keys this scenario decides on. Names the
    /// scenario in a violation detail and, through its idempotency keys and
    /// its meter, in the ledger.
    role: &'static str,
    /// The meter this scenario is written to and the only meter it reads.
    meter: MeterTypeId,
    /// The range its fold dispatches.
    range: TimeRange,
    /// The compiled scope its fold dispatches, which must admit every entry
    /// below and nothing else.
    scope: ast::Expr,
    /// The entries, **in submission order**. The order is part of the
    /// fixture; see [`tie_break_scenarios`].
    submissions: Vec<UsageRecord>,
    /// The entry DESIGN's order selects.
    winner: UsageRecord,
    /// [`Self::winner`]'s quantity on the aggregate surface's carrier.
    expected: BigDecimal,
    /// The entry DESIGN's order does not select, and which plausible backend
    /// picks it instead.
    loser: UsageRecord,
    /// What the scenario establishes, spelled for a plugin author reading a
    /// violation.
    ///
    /// An owned string rather than a literal: two of the three name the
    /// attempt index their inversion was found at, which is the one fact a
    /// reader needs in order to reproduce the pair.
    why: String,
}

/// `latest-tie-break` — *"The §3.1 order, including two entries sharing a
/// period and an aggregate spanning tenants."* (DESIGN §3.3, "Plugin
/// contract tests", line 1327.)
///
/// **That row is a pointer rather than the rule.** The order itself is
/// §3.1's `LATEST` tie-break invariant, at line 599: *"Greatest `window_end`,
/// then greatest `accepted_at`, then greatest `id` in byte order. `id` is
/// unique, so the order is total, and all three keys compare across tenants
/// and types, so it also holds for a group spanning tenants. `MAX` and `MIN`
/// need no such rule."*
///
/// Three keys, and one scenario decides on each. The row names two of them
/// and the order implies the third:
///
/// 1. **`window_end` decides.** Two entries whose covered periods end an hour
///    apart. The later-ending one wins *whatever* its other two keys say, so
///    it is built carrying the smaller `accepted_at` **and** the smaller
///    `id`: a backend that reads either of the lower keys first reports the
///    other entry.
/// 2. **`accepted_at` decides** - the row's *"two entries sharing a period"*.
///    One covered period, two acceptance instants an hour apart. The
///    later-accepted entry wins, and it too carries the smaller `id`, which
///    is what stops the scenario passing by construction: a fold keyed on
///    `(window_end, id)` and one keyed on all three pick the same winner
///    unless the middle key and the last disagree.
/// 3. **`id` decides, across tenants** - the row's *"aggregate spanning
///    tenants"*. Two entries agreeing on `window_end` **and** on
///    `accepted_at`, under two different tenants, folded in one ungrouped
///    aggregate whose scope admits both. The greater `id` wins. This is the
///    scenario that makes the order *total*, which is the property §3.1
///    states `id`'s uniqueness for, and it is the one no single-tenant
///    fixture can reach.
///
/// **Two kinds of inversion, and neither can be chosen.** The first is the
/// `id` order, and the two scenarios that decide above `id` both need it.
/// An `id` is the `UUIDv5` over the six identity inputs
/// ([`derive_usage_record_id`](crate::id::derive_usage_record_id)), so which
/// of two entries carries the greater one is a property of the derived values
/// rather than something a fixture picks. [`inverted_id_pair`] searches for
/// it over indexed idempotency keys and fails loudly as a
/// [`HARNESS_FAULT`] if it never inverts, which is the only honest thing to
/// do: a scenario built on an inversion that did not happen asserts nothing
/// and reports nothing. The cross-tenant scenario needs no search, because
/// there the greater `id` *is* the winner: it reads the pair rather than
/// searching it.
///
/// The other inversion is the **submission order**, and it is why
/// [`TieBreakScenario::submissions`] is a sequence rather than a set.
/// Arrival order is not one of DESIGN's three keys, and a backend that
/// substitutes it for one of them - ordering by an insertion counter instead
/// of by `accepted_at`, say - ranks the *last* arrival highest, because that
/// is what any counter meant to say "latest" does. So every scenario here
/// submits the entry DESIGN's order selects **first** and the entry it
/// rejects last, which makes arrival order the exact opposite of DESIGN's
/// over each pair. A fixture whose arrival order agreed with DESIGN's would
/// let such a backend answer correctly by accident, which is the second way
/// a scenario here can pass while asserting nothing.
///
/// That is not a hypothetical backend. The `TimescaleDB` plugin orders
/// `LATEST` by `window_end DESC, acceptance_sequence DESC`, and
/// `acceptance_sequence` is a per-`(tenant_id, gts_type_id)` counter claimed
/// at insert: it is arrival order inside one tenant and one meter, and no
/// order at all across a group spanning tenants. The `accepted_at` scenario
/// is what puts arrival order and DESIGN's middle key in opposition, and the
/// cross-tenant scenario is what asks for an order the counter cannot
/// supply.
///
/// Each scenario is asserted separately, and they fail independently. One
/// combined assertion would pass against a backend that got two keys wrong in
/// compensating directions, which is the failure a three-key order is stated
/// to prevent.
///
/// **What this check does not reach.** `MAX` and `MIN` are dispatched
/// nowhere here, on §3.1's own authority: they *"need no such rule"*, because
/// they select on the quantity and a tie between two equal quantities cannot
/// be observed through the value a fold reports. Nor does anything here read
/// a **grouped** aggregate: DESIGN puts the rule on the group, and a group is
/// exactly what the ungrouped bucket is a degenerate case of, so a backend
/// whose grouped path took a different route to its winner would pass every
/// fold here.
pub async fn latest_tie_break(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let scenarios = match tie_break_scenarios() {
        Ok(scenarios) => scenarios,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{LATEST_TIE_BREAK}` fixtures, so \
                     nothing was submitted. This is a fault in the suite, not in the plugin under \
                     test: {detail}"
                ),
            )];
        }
    };

    let mut violations = Vec::new();
    for scenario in &scenarios {
        violations.extend(one_key_of_the_order(plugin, scenario).await);
    }
    violations
}

/// Stores one scenario's entries in their submission order and folds over
/// them.
///
/// A refusal stops **this** scenario and not the other two: each decides a
/// different key of the order and a backend that cannot store one pair may
/// still be asked about the others. It is reported under
/// [`LATEST_TIE_BREAK`] rather than as a harness fault, because the
/// submission is the plugin's to accept.
async fn one_key_of_the_order(
    plugin: &dyn UsageCollectorPluginV1,
    scenario: &TieBreakScenario,
) -> Vec<ContractViolation> {
    for record in &scenario.submissions {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                LATEST_TIE_BREAK,
                format!(
                    "`create_usage_record` refused an entry of the `{role}` scenario (record \
                     {id}, tenant {tenant}, covered period `{start}` to `{end}`), so the key of \
                     DESIGN section 3.1's `LATEST` order it decides was never asserted: {err}",
                    role = scenario.role,
                    id = record.id,
                    tenant = record.tenant_id,
                    start = record.window_start,
                    end = record.window_end,
                ),
            )];
        }
    }

    let observed = match ungrouped_latest(plugin, scenario).await {
        Ok(observed) => observed,
        Err(detail) => return vec![violation(LATEST_TIE_BREAK, detail)],
    };
    if observed.as_ref() == Some(&scenario.expected) {
        return Vec::new();
    }

    vec![violation(
        LATEST_TIE_BREAK,
        format!(
            "an ungrouped `LATEST` over the range `[{from}, {to})` reported {observed}, and \
             DESIGN section 3.1 fixes the order as greatest `window_end`, then greatest \
             `accepted_at`, then greatest `id` in byte order. Over this scenario's two entries \
             that order selects record {winner} and its quantity `{expected}`; the other entry \
             is record {loser}, carrying `{loser_quantity}`. {why} The two entries were \
             submitted in the order {submitted:?}, the entry DESIGN ranks highest first. That \
             order is part of the fixture: a backend that substitutes arrival order for one of \
             the three keys, or that runs out of keys and settles what is left on arrival, \
             ranks the last arrival highest and so reports the entry DESIGN's order rejects \
             rather than agreeing with the rule by accident.",
            from = scenario.range.lower_inclusive(),
            to = scenario.range.upper_exclusive(),
            observed = observed
                .as_ref()
                .map_or_else(|| "no value at all".to_owned(), ToString::to_string),
            winner = scenario.winner.id,
            expected = scenario.expected,
            loser = scenario.loser.id,
            loser_quantity = scenario.loser.quantity,
            why = scenario.why,
            submitted = scenario
                .submissions
                .iter()
                .map(|record| record.id)
                .collect::<Vec<Uuid>>(),
        ),
    )]
}

/// The `LATEST` one bucket carries over one scenario's range, meter and
/// scope.
///
/// `group_by` is empty, which the aggregate surface fixes as the no-grouping
/// case: a single bucket with an empty key. A result carrying any other
/// number of buckets is reported rather than picked from, since there would
/// be no one winner to compare. `Err` carries a ready-to-report detail.
async fn ungrouped_latest(
    plugin: &dyn UsageCollectorPluginV1,
    scenario: &TieBreakScenario,
) -> Result<Option<BigDecimal>, String> {
    let query = contract_query_with_scope(LATEST_TIE_BUCKET_LIMIT, scenario.scope.clone());
    let result = plugin
        .query_aggregated_usage_records(
            scenario.meter.clone(),
            scenario.range,
            AggregationFold::Latest,
            &query,
            &[],
            &[],
        )
        .await
        .map_err(|err| {
            format!(
                "`query_aggregated_usage_records` failed under `LATEST` over the range holding \
                 the `{role}` scenario's two entries, so the key of DESIGN section 3.1's order \
                 they decide could not be read: {err}",
                role = scenario.role,
            )
        })?;
    let count = result.buckets.len();
    let mut buckets = result.buckets.into_iter();
    match (buckets.next(), buckets.next()) {
        (Some(bucket), None) => Ok(bucket.value),
        _ => Err(format!(
            "`query_aggregated_usage_records` was dispatched under `LATEST` with no grouping \
             dimension, which the aggregate surface fixes as the no-grouping case - a single \
             bucket carrying an empty key - and it answered {count} buckets over the \
             `{role}` scenario. There is no one winner to hold DESIGN section 3.1's order to.",
            role = scenario.role,
        )),
    }
}

/// Builds the three scenarios, one per key of DESIGN §3.1's order.
///
/// **Two meters rather than one.** The `window_end` and `accepted_at`
/// scenarios share [`check_meter`]`(LATEST_TIE_BREAK, "main")` and are
/// separated from each other by covered periods no range of the other
/// selects. The cross-tenant scenario takes a meter of its own, for the
/// reason [`converged_target_lookup`](super::converged_target_lookup())
/// separates by meter: its entries are the only ones this suite writes under
/// the tenants [`contract_tenant`] mints, and `super::super::run_all`
/// dispatches every check against one backend that never removes an entry,
/// so an entry of theirs on a shared meter is an entry some other check's
/// page or fold has to account for. Nothing here depends on that separation,
/// because the scopes and the ranges already keep the three folds apart in
/// both directions; it is kept because the direction it runs in is the one
/// no assertion in this check can see.
///
/// Every range holds **both** covered-period bounds of every entry it
/// selects. This check asserts nothing about period selection, and a range
/// that began where an entry's period ends would make its fold unanswerable
/// for a backend selecting on `window_start` - coupling this check to
/// `window-end-selection`'s rule for no reason of its own.
///
/// Guards keep each scenario from passing by construction, and all of them
/// are the suite's own facts rather than the plugin's, so all are reported as
/// [`HARNESS_FAULT`]. They are asserted in [`TieBreakScenario::guards`].
fn tie_break_scenarios() -> Result<Vec<TieBreakScenario>, String> {
    let main = check_meter(LATEST_TIE_BREAK, "main")?;
    let across_tenants = check_meter(LATEST_TIE_BREAK, "across-tenants")?;

    let scenarios = vec![
        the_period_end_decides(&main)?,
        the_acceptance_instant_decides(&main)?,
        the_identifier_decides_across_tenants(&across_tenants)?,
    ];
    for scenario in &scenarios {
        scenario.guards()?;
    }
    Ok(scenarios)
}

/// Scenario one: two covered periods an hour apart, and the later end wins
/// against both lower keys at once.
///
/// The winner carries the smaller `accepted_at` by construction and the
/// smaller `id` by search, so a backend ordering on `accepted_at` alone -
/// *"latest is whatever was accepted last"*, the most natural misreading of
/// the fold's name - reports the loser, and so does one ordering on `id`
/// alone.
fn the_period_end_decides(meter: &MeterTypeId) -> Result<TieBreakScenario, String> {
    let loser_end = LATEST_TIE_FROM.saturating_add(time::Duration::hours(1));
    let winner_end = LATEST_TIE_FROM.saturating_add(time::Duration::hours(2));
    let (winner, loser, attempt) = inverted_id_pair(
        "The `window_end` scenario needs its later-ending entry under the smaller `id`, so that \
         the entry DESIGN's first key selects is the entry a fold reading `id` alone rejects.",
        |attempt| {
            Ok((
                tie_break_entry(
                    meter,
                    CONTRACT_TENANT_ID,
                    &tie_break_key("window-end-winner", attempt)?,
                    WINDOW_END_WINNER_QUANTITY,
                    CONTRACT_ACCEPTED_AT,
                    loser_end,
                    winner_end,
                )?,
                tie_break_entry(
                    meter,
                    CONTRACT_TENANT_ID,
                    &tie_break_key("window-end-loser", attempt)?,
                    WINDOW_END_LOSER_QUANTITY,
                    LATEST_TIE_LATER_ACCEPTED_AT,
                    LATEST_TIE_FROM,
                    loser_end,
                )?,
            ))
        },
    )?;

    Ok(TieBreakScenario {
        role: "window-end",
        meter: meter.clone(),
        range: tie_break_range(LATEST_TIE_FROM, winner_end)?,
        scope: contract_scope(),
        submissions: vec![winner.clone(), loser.clone()],
        winner,
        expected: widen(WINDOW_END_WINNER_QUANTITY)?,
        loser,
        why: format!(
            "The two periods end an hour apart, and the later-ending entry is the one DESIGN's \
             first key selects whatever its other two keys say. It carries the smaller \
             `accepted_at` and the smaller `id` (the pair found at attempt {attempt}), so a \
             backend that reads either lower key before `window_end` reports the other entry: \
             ordering by acceptance instant alone is the ordinary misreading of a fold named \
             `LATEST`, and ordering by identifier alone is what a fold keyed on the ledger's \
             primary key does."
        ),
    })
}

/// Scenario two: one covered period, two acceptance instants, and the
/// later-accepted entry under the smaller `id`.
///
/// This is the row's *"two entries sharing a period"*, and the `id` inversion
/// is the whole of what makes it able to fail: with the two lower keys
/// agreeing, a fold keyed on `(window_end, id)` picks the same winner as one
/// keyed on all three.
fn the_acceptance_instant_decides(meter: &MeterTypeId) -> Result<TieBreakScenario, String> {
    let start = LATEST_TIE_FROM.saturating_add(time::Duration::hours(4));
    let end = LATEST_TIE_FROM.saturating_add(time::Duration::hours(5));
    let (winner, loser, attempt) = inverted_id_pair(
        "The `accepted_at` scenario needs its later-accepted entry under the smaller `id`. The \
         two share a `window_end`, so with the middle key and the last agreeing a fold that \
         skips the middle one picks the same entry as a conforming fold and the scenario proves \
         nothing.",
        |attempt| {
            Ok((
                tie_break_entry(
                    meter,
                    CONTRACT_TENANT_ID,
                    &tie_break_key("accepted-at-winner", attempt)?,
                    ACCEPTED_AT_WINNER_QUANTITY,
                    LATEST_TIE_LATER_ACCEPTED_AT,
                    start,
                    end,
                )?,
                tie_break_entry(
                    meter,
                    CONTRACT_TENANT_ID,
                    &tie_break_key("accepted-at-loser", attempt)?,
                    ACCEPTED_AT_LOSER_QUANTITY,
                    CONTRACT_ACCEPTED_AT,
                    start,
                    end,
                )?,
            ))
        },
    )?;

    Ok(TieBreakScenario {
        role: "accepted-at",
        meter: meter.clone(),
        range: tie_break_range(start, end)?,
        scope: contract_scope(),
        submissions: vec![winner.clone(), loser.clone()],
        winner,
        expected: widen(ACCEPTED_AT_WINNER_QUANTITY)?,
        loser,
        why: format!(
            "The two entries share a covered period, so the first key ties and the tie-break \
             has to reach the second. The later-accepted entry carries the smaller `id` (the \
             pair found at attempt {attempt}), so a fold that falls from `window_end` straight \
             through to `id` - which is what this suite's own reference backend did until its \
             fold was corrected - reports the earlier-accepted entry instead."
        ),
    })
}

/// Scenario three: two tenants, every key above `id` tied, and one ungrouped
/// aggregate whose scope admits both.
///
/// No search is needed and none would help: the winner is *defined* as the
/// greater `id`, so the pair is read rather than searched for. What the
/// fixture chooses instead is the submission order, descending by `id`, so
/// that a backend whose order runs out after `accepted_at` and settles the
/// remainder on arrival picks the smaller `id` and is reported.
fn the_identifier_decides_across_tenants(meter: &MeterTypeId) -> Result<TieBreakScenario, String> {
    let start = LATEST_TIE_FROM.saturating_add(time::Duration::hours(7));
    let end = LATEST_TIE_FROM.saturating_add(time::Duration::hours(8));
    // Attempt index zero, fixed: the key carries one because the other two
    // scenarios search over it, and this scenario has nothing to search for.
    let entry = |index: u32, role: &str, quantity: &str| {
        tie_break_entry(
            meter,
            contract_tenant(index),
            &tie_break_key(role, 0)?,
            quantity,
            CONTRACT_ACCEPTED_AT,
            start,
            end,
        )
    };
    let first = entry(
        ACROSS_TENANTS_FIRST_TENANT,
        "across-tenants-first",
        ACROSS_TENANTS_WINNER_QUANTITY,
    )?;
    let second = entry(
        ACROSS_TENANTS_SECOND_TENANT,
        "across-tenants-second",
        ACROSS_TENANTS_LOSER_QUANTITY,
    )?;

    // The greater `id` is the winner, and which of the two carries it is a
    // property of the derived values. The quantities travel with the roles
    // rather than with the outcome, so whichever entry wins is read back
    // through its own quantity.
    let (winner, loser) = if first.id > second.id {
        (first, second)
    } else {
        (second, first)
    };
    let expected = widen(&winner.quantity.to_string())?;

    Ok(TieBreakScenario {
        role: "across-tenants",
        meter: meter.clone(),
        range: tie_break_range(start, end)?,
        scope: two_tenant_scope(winner.tenant_id, loser.tenant_id),
        submissions: vec![winner.clone(), loser.clone()],
        winner,
        expected,
        loser,
        why: "The two entries agree on `window_end` and on `accepted_at` and sit under two \
              different tenants, folded in one ungrouped aggregate whose compiled scope admits \
              both. Only the third key separates them, and DESIGN states `id`'s uniqueness \
              precisely so that it can: the order is total, and all three keys compare across \
              tenants and types, so a group spanning tenants is ordered by the same rule as one \
              inside a single tenant. A backend whose order runs out before `id` - one keyed on \
              a per-tenant insertion counter, say, which orders nothing across a group like this \
              one - has no answer here and settles the tie however its scan happened to arrive."
            .to_owned(),
    })
}

/// The compiled scope the cross-tenant fold dispatches: `tenant_id eq <a> or
/// tenant_id eq <b>`.
///
/// Built from the two entries' own tenant ids rather than from the constants
/// they were minted with, so a scenario edited to move an entry to a third
/// tenant cannot leave behind a scope that no longer admits it.
///
/// This is the shape `authz::scope_to_odata_filter` projects for a grant
/// carrying two single-tenant constraints, and the same shape
/// `scope-is-a-filter-on-every-read-path` dispatches for its own reasons.
fn two_tenant_scope(first: Uuid, second: Uuid) -> ast::Expr {
    ast::Expr::Or(Box::new(tenant_is(first)), Box::new(tenant_is(second)))
}

/// One disjunct of [`two_tenant_scope`].
fn tenant_is(tenant_id: Uuid) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(tenant_id))),
    )
}

impl TieBreakScenario {
    /// The facts a scenario needs in order to assert anything, checked
    /// against the entries as they were actually built.
    ///
    /// Each guard closes a way for the scenario to pass whatever the plugin
    /// does:
    ///
    /// * **The two quantities differ.** If they did not, a fold picking
    ///   either entry would report the same number and no order could be
    ///   distinguished from any other.
    /// * **The two entries derive different ids.** They would otherwise be
    ///   one identity, the second submission an idempotent replay of the
    ///   first, and the fold would meet a single row.
    /// * **The scope admits both entries.** The cross-tenant scenario builds
    ///   a scope of its own and the other two take the suite's; a scope that
    ///   withheld one entry would leave the fold with one candidate and no
    ///   tie to break.
    /// * **The range selects both entries by period end**, and holds both
    ///   period starts as well. The first is what puts them in one fold; the
    ///   second is what keeps this check off `window-end-selection`'s rule.
    /// * **The winner is the entry DESIGN's order selects**, recomputed here
    ///   from the three keys rather than taken from whichever branch built
    ///   the scenario. This is the guard that would catch a scenario whose
    ///   inversion silently stopped holding.
    fn guards(&self) -> Result<(), String> {
        if self.winner.quantity == self.loser.quantity {
            return Err(format!(
                "the `{role}` scenario's two entries carry one quantity ({quantity}), so a fold \
                 picking either of them reports the same number and no order can be told from \
                 any other",
                role = self.role,
                quantity = self.winner.quantity,
            ));
        }
        if self.winner.id == self.loser.id {
            return Err(format!(
                "the `{role}` scenario's two entries derive one id ({id}), so they are one \
                 identity rather than two entries: the second submission is an idempotent replay \
                 of the first and the fold meets a single row, with no tie to break",
                role = self.role,
                id = self.winner.id,
            ));
        }
        for (side, record) in [("winner", &self.winner), ("loser", &self.loser)] {
            if !self.range.contains_window_end(record.window_end) {
                return Err(format!(
                    "the `{role}` scenario's range `[{from}, {to})` does not select its {side} \
                     (record {id}, covered period `{start}` to `{end}`) by period end, so the \
                     fold this scenario dispatches would meet at most one of its two entries",
                    role = self.role,
                    from = self.range.lower_inclusive(),
                    to = self.range.upper_exclusive(),
                    id = record.id,
                    start = record.window_start,
                    end = record.window_end,
                ));
            }
            if record.window_start < self.range.lower_inclusive()
                || record.window_start >= self.range.upper_exclusive()
            {
                return Err(format!(
                    "the `{role}` scenario's range `[{from}, {to})` does not hold its {side}'s \
                     period start (`{start}`, record {id}). Every range here holds both bounds \
                     of every entry it selects, so that a backend selecting on `window_start` \
                     meets the same two entries as a conforming one and this check stays clear \
                     of `window-end-selection`'s rule",
                    role = self.role,
                    from = self.range.lower_inclusive(),
                    to = self.range.upper_exclusive(),
                    start = record.window_start,
                    id = record.id,
                ));
            }
        }
        if let Some(missing) = self.scope_withholds() {
            return Err(format!(
                "the `{role}` scenario dispatches a compiled scope that does not admit its own \
                 entry {missing} (tenant {tenant}), so the fold would be answered over one \
                 candidate and there would be no tie to break",
                role = self.role,
                tenant = if missing == self.winner.id {
                    self.winner.tenant_id
                } else {
                    self.loser.tenant_id
                },
            ));
        }
        let declared = declared_order_winner(&self.winner, &self.loser);
        if declared != self.winner.id {
            return Err(format!(
                "the `{role}` scenario names record {named} as the entry DESIGN section 3.1's \
                 order selects, and that order - greatest `window_end`, then greatest \
                 `accepted_at`, then greatest `id` in byte order - selects record {declared} \
                 over these two entries. The scenario would hold a conforming backend to the \
                 wrong answer",
                role = self.role,
                named = self.winner.id,
            ));
        }
        Ok(())
    }

    /// The id of an entry this scenario's own scope does not admit, if there
    /// is one.
    ///
    /// The scope is read for the tenant ids it names, which is the only
    /// predicate any scope here carries: the suite's own is `tenant_id eq
    /// <tenant>` and the cross-tenant scenario's is two of those under an
    /// `or`. A scope of any other shape is treated as admitting nothing,
    /// which fails the guard rather than passing it silently.
    fn scope_withholds(&self) -> Option<Uuid> {
        let admitted = scope_tenants(&self.scope);
        if !admitted.contains(&self.winner.tenant_id) {
            return Some(self.winner.id);
        }
        if !admitted.contains(&self.loser.tenant_id) {
            return Some(self.loser.id);
        }
        None
    }
}

/// Every tenant id a scope of the two shapes this check dispatches names.
///
/// Deliberately narrow. It is a guard over the suite's own fixtures rather
/// than a filter evaluator, and a wider reader would quietly start admitting
/// shapes no scenario here builds.
fn scope_tenants(scope: &ast::Expr) -> Vec<Uuid> {
    match scope {
        ast::Expr::Or(left, right) => {
            let mut tenants = scope_tenants(left);
            tenants.extend(scope_tenants(right));
            tenants
        }
        ast::Expr::Compare(field, ast::CompareOperator::Eq, value) => {
            match (field.as_ref(), value.as_ref()) {
                (ast::Expr::Identifier(name), ast::Expr::Value(ast::Value::Uuid(tenant)))
                    if name == "tenant_id" =>
                {
                    vec![*tenant]
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// The entry DESIGN §3.1's order selects out of two.
///
/// Spelled here as the three keys in order rather than delegated to a
/// backend, because it is what the guards hold a scenario's declared winner
/// against. `id` is compared as bytes by construction:
/// [`Uuid`] is a `#[repr(transparent)]` newtype over `[u8; 16]` carrying a
/// derived `Ord`, so comparing two of them compares those sixteen bytes
/// lexicographically.
fn declared_order_winner(left: &UsageRecord, right: &UsageRecord) -> Uuid {
    let key = |record: &UsageRecord| (record.window_end, record.accepted_at, record.id);
    if key(left) >= key(right) {
        left.id
    } else {
        right.id
    }
}

/// Builds one entry of one scenario.
fn tie_break_entry(
    meter: &MeterTypeId,
    tenant_id: Uuid,
    idempotency_key: &IdempotencyKey,
    quantity: &str,
    accepted_at: time::OffsetDateTime,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<UsageRecord, String> {
    let quantity = UsageQuantity::parse(quantity)
        .map_err(|err| format!("the check's own quantity `{quantity}` does not parse: {err}"))?;
    fixture_record_on(
        meter.clone(),
        tenant_id,
        idempotency_key,
        quantity,
        accepted_at,
        window_start,
        window_end,
    )
}

/// The range one scenario folds over: from the earliest period start to an
/// hour past the latest period end.
///
/// The margin is what holds every entry's period **start** inside the range
/// as well as its end. [`TieBreakScenario::guards`] says why that matters.
fn tie_break_range(
    earliest_start: time::OffsetDateTime,
    latest_end: time::OffsetDateTime,
) -> Result<TimeRange, String> {
    TimeRange::new(
        earliest_start,
        latest_end.saturating_add(time::Duration::hours(1)),
    )
    .map_err(|err| format!("the check could not build its own read range: {err}"))
}

/// One entry's quantity on the aggregate surface's carrier.
fn widen(quantity: &str) -> Result<BigDecimal, String> {
    BigDecimal::from_str(quantity).map_err(|err| {
        format!(
            "the check's own quantity `{quantity}` does not widen to the aggregate carrier: {err}"
        )
    })
}

/// The idempotency key one entry of one scenario submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) spells
/// out: in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's. The `attempt` index is
/// the one input [`inverted_id_pair`] is free to vary, and it is part of the
/// key for the same reason: a search that settles on a different index is a
/// different pair of entries rather than a mutation of the one already
/// accepted.
fn tie_break_key(role: &str, attempt: u32) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{LATEST_TIE_BREAK}-{role}-{attempt}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
