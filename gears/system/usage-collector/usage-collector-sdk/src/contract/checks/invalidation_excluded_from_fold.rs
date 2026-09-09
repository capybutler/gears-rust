//! The DESIGN §3.3 `invalidation-excluded-from-fold` check.
//!
//! See [`invalidation_excluded_from_fold`] for what it asserts; the module
//! holds the withdrawn pair, the live entry beside it, and the two halves —
//! the fold and the ledger read — that are asserted over them.

use std::collections::BTreeSet;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_METER_TYPE_ID, FIXTURE_EPOCH, contract_query, fixture_invalidation, fixture_record,
    violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, INVALIDATION_EXCLUDED_FROM_FOLD};
use crate::models::{AggregationFold, IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

/// The start of the live entry's covered period, and the inclusive lower
/// bound of the range this check folds and reads over.
///
/// Ninety days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`](super::window_end_selection::WINDOW_SELECTION_FROM)
/// gives. It matters here for the same reason it matters there: this check
/// counts the rows a range returns, so a stray entry from another check
/// inside it would be read as a fourth entry.
const FOLD_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(90));

/// The exclusive upper bound of that range, an hour past the end of the
/// withdrawn pair's period so all three entries are selected by their end.
const FOLD_WINDOW_TO: time::OffsetDateTime =
    FOLD_WINDOW_FROM.saturating_add(time::Duration::hours(3));

/// The live entry's quantity, and the whole of the total the fold must
/// report.
///
/// Distinct from [`FOLD_WITHDRAWN_QUANTITY`] and small beside it, so the
/// three answers a backend can give are three different numbers:
/// `FOLD_LIVE_QUANTITY` alone when the withdrawn pair is excluded,
/// `FOLD_LIVE_QUANTITY + FOLD_WITHDRAWN_QUANTITY` when only the record is,
/// and `FOLD_LIVE_QUANTITY + 2 × FOLD_WITHDRAWN_QUANTITY` when neither is.
/// Stated as arithmetic over the two constants rather than as the literal
/// sums, so editing either quantity cannot leave the numbers here behind.
const FOLD_LIVE_QUANTITY: &str = "7.25";

/// The withdrawn record's quantity, and the quantity the invalidation that
/// withdraws it carries.
///
/// An invalidation **echoes** the quantity it withdraws rather than
/// negating it (`cpt-cf-usage-collector-adr-append-only-invalidation`),
/// which is exactly why leaving out only the record double-counts instead
/// of cancelling: the echoed term stays in the sum with nothing left to
/// pair it against.
const FOLD_WITHDRAWN_QUANTITY: &str = "1000";

/// The read limit this check dispatches: twice the three entries it
/// expects.
///
/// The margin is the assertion, for the reason
/// [`DEDUP_PAGE_LIMIT`](super::dedup_identity_over_window::DEDUP_PAGE_LIMIT)
/// gives. A limit set to the expected three would truncate a fourth row
/// away, and
/// the half of this check that catches a backend storing something extra
/// would pass.
const FOLD_PAGE_LIMIT: u64 = 6;

/// The three entries this check submits, and the total the fold must report
/// over them.
struct FoldFixtures {
    /// The surviving measurement, and the only entry the fold may count.
    live: UsageRecord,
    /// The measurement the invalidation withdraws.
    withdrawn: UsageRecord,
    /// The invalidation entry naming [`Self::withdrawn`].
    invalidation: UsageRecord,
    /// [`Self::live`]'s quantity on the aggregate surface's carrier.
    expected_total: BigDecimal,
}

/// `invalidation-excluded-from-fold` — *"A withdrawn pair folds to nothing
/// while both entries stay readable. Excluding only the record
/// double-counts the withdrawn measurement."*
///
/// **Both sentences are the rule, and the second is the easy half to
/// lose.** They are two obligations rather than one conditional
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`): the fold leaves
/// out the invalidation *and* the record it names, and the raw path leaves
/// out neither.
///
/// * Excluding the record but folding the invalidation double-counts the
///   withdrawn measurement, because the invalidation echoes the quantity it
///   withdraws rather than negating it. There is no term that cancels.
/// * Excluding both from the **raw** path instead would make the ledger
///   unauditable. The append-only correction model exists precisely so a
///   withdrawal is *visible* rather than a deletion, which is why
///   `get_usage_record` and `list_usage_records` return a withdrawn pair as
///   persisted and the exclusion lives in the fold alone.
///
/// The fixture is one live entry with a distinctive quantity, plus a second
/// entry and the invalidation that withdraws it.
///
/// **The `SUM` is asserted against the surviving entry's value, never
/// against zero.** A `SUM` over a withdrawn pair on its own comes back
/// empty from a backend that folds correctly and from one that computes no
/// fold at all — the latter is what the noop plugin does, returning no
/// buckets whatsoever — so an assertion phrased as "the withdrawn pair
/// contributes nothing" would be satisfied by a backend that answers
/// nothing. The live entry is what makes the assertion discriminate: the
/// fold must report [`FOLD_LIVE_QUANTITY`] exactly, not zero, and not that
/// value plus or minus [`FOLD_WITHDRAWN_QUANTITY`].
///
/// The raw half then asserts three entries come back over the same range
/// and that the invalidation among them still names its target. The two
/// halves fail independently: a backend that hides the withdrawn pair from
/// `list_usage_records` folds correctly and fails only the second, and one
/// that folds the invalidation in returns all three rows and fails only the
/// first.
pub async fn invalidation_excluded_from_fold(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match fold_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own \
                     `{INVALIDATION_EXCLUDED_FROM_FOLD}` fixtures, so nothing was submitted. This \
                     is a fault in the suite, not in the plugin under test: {detail}"
                ),
            )];
        }
    };

    // A refusal here is reported and the check stops. Unlike the fixture
    // rows of `window-end-selection`, these three are one scenario: a fold
    // asserted over a pair that was never admitted, or over a live entry
    // that was not, would report a total that says nothing about the
    // exclusion rule.
    for (role, record) in [
        ("live", &fixtures.live),
        ("withdrawn", &fixtures.withdrawn),
        ("invalidation", &fixtures.invalidation),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            return vec![violation(
                INVALIDATION_EXCLUDED_FROM_FOLD,
                format!(
                    "`create_usage_record` refused the {role} entry (record {id}), so neither the \
                     fold nor the ledger read could be asserted over a withdrawn pair: {err}",
                    id = record.id,
                ),
            )];
        }
    }

    let mut violations = fold_excludes_the_pair(plugin, &fixtures).await;
    violations.extend(ledger_keeps_the_pair(plugin, &fixtures).await);
    violations
}

/// The fold half: the withdrawn pair contributes nothing, and the live
/// entry's quantity is the whole of the total.
async fn fold_excludes_the_pair(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FoldFixtures,
) -> Vec<ContractViolation> {
    match fold_sum(plugin).await {
        Ok(Some(total)) if total == fixtures.expected_total => Vec::new(),
        Ok(observed) => vec![violation(
            INVALIDATION_EXCLUDED_FROM_FOLD,
            format!(
                "`SUM` over the range `[{from}, {to})` reported {observed}, and the live entry's \
                 own quantity `{expected}` is the whole of it. The range holds that entry, a \
                 second entry of `{withdrawn}`, and the invalidation withdrawing it - which \
                 carries the same `{withdrawn}` rather than its negation. An invalidation entry \
                 contributes nothing to any fold, and so does the record an accepted \
                 invalidation names: leaving out only the record leaves the echoed `{withdrawn}` \
                 in the total and double-counts the withdrawn measurement, and leaving out \
                 neither counts it twice over. The comparison is against the surviving entry \
                 rather than against zero on purpose - a fold over the withdrawn pair alone is \
                 empty whether a backend excluded it correctly or computed no fold at all.",
                from = FOLD_WINDOW_FROM,
                to = FOLD_WINDOW_TO,
                observed = observed
                    .as_ref()
                    .map_or_else(|| "no value at all".to_owned(), ToString::to_string),
                expected = FOLD_LIVE_QUANTITY,
                withdrawn = FOLD_WITHDRAWN_QUANTITY,
            ),
        )],
        Err(detail) => vec![violation(INVALIDATION_EXCLUDED_FROM_FOLD, detail)],
    }
}

/// The ledger half: all three entries stay readable on the raw path, and
/// the invalidation among them still names its target.
///
/// Both are what make the correction model auditable rather than a
/// deletion. A consumer folding entries it read here has to be able to see
/// the pair *and* to tell which record was withdrawn, and the target
/// reference is the only thing that says so — an entry known to be an
/// invalidation still has to name the record it withdrew before anything
/// can be left out.
async fn ledger_keeps_the_pair(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &FoldFixtures,
) -> Vec<ContractViolation> {
    let items = match fold_page(plugin).await {
        Ok(items) => items,
        Err(detail) => return vec![violation(INVALIDATION_EXCLUDED_FROM_FOLD, detail)],
    };

    let mut violations = Vec::new();
    let returned: BTreeSet<Uuid> = items.iter().map(|item| item.id).collect();
    let expected = BTreeSet::from([
        fixtures.live.id,
        fixtures.withdrawn.id,
        fixtures.invalidation.id,
    ]);
    if items.len() != expected.len() || returned != expected {
        violations.push(violation(
            INVALIDATION_EXCLUDED_FROM_FOLD,
            format!(
                "the range `[{from}, {to})` holds a live entry ({live}), a withdrawn entry \
                 ({withdrawn}) and the invalidation withdrawing it ({invalidation}), and \
                 `list_usage_records` answered a row count of {count} over the ids {returned:?}. \
                 A withdrawn pair is returned as persisted on the ledger paths - both entries - \
                 because the append-only correction model exists so a withdrawal is visible \
                 rather than a deletion. Withholding one destroys the audit trail; the exclusion \
                 belongs to the fold alone.",
                from = FOLD_WINDOW_FROM,
                to = FOLD_WINDOW_TO,
                live = fixtures.live.id,
                withdrawn = fixtures.withdrawn.id,
                invalidation = fixtures.invalidation.id,
                count = items.len(),
            ),
        ));
    }

    // Only asserted when the entry came back at all: its absence is
    // already reported above, and reporting it twice would read as two
    // defects.
    if let Some(entry) = items
        .iter()
        .find(|item| item.id == fixtures.invalidation.id)
    {
        let target = entry.invalidation.as_ref().map(|inv| inv.target);
        if target != Some(fixtures.withdrawn.id) {
            violations.push(violation(
                INVALIDATION_EXCLUDED_FROM_FOLD,
                format!(
                    "the invalidation entry ({invalidation}) came back from \
                     `list_usage_records` naming {observed} as the entry it withdraws, and it \
                     withdraws {withdrawn}. The target reference is what makes the pair \
                     auditable: it is what marks this entry an invalidation at all, and it is \
                     the only thing that says which record was withdrawn, so a consumer folding \
                     entries it read here cannot leave the pair out without it.",
                    invalidation = fixtures.invalidation.id,
                    observed = target.map_or_else(
                        || "no entry at all".to_owned(),
                        |target| format!("`{target}`")
                    ),
                    withdrawn = fixtures.withdrawn.id,
                ),
            ));
        }
    }

    violations
}

/// The `SUM` one bucket carries over the range under test.
///
/// `group_by` is empty, which the aggregate surface fixes as the
/// no-grouping case: a single bucket with an empty key. A result carrying
/// any other number of buckets is reported rather than picked from, since
/// there would be no one total to compare. `Err` carries a ready-to-report
/// detail.
async fn fold_sum(plugin: &dyn UsageCollectorPluginV1) -> Result<Option<BigDecimal>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(FOLD_WINDOW_FROM, FOLD_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let result = plugin
        .query_aggregated_usage_records(
            meter,
            range,
            AggregationFold::Sum,
            &contract_query(FOLD_PAGE_LIMIT),
            &[],
            &[],
        )
        .await
        .map_err(|err| {
            format!(
                "`query_aggregated_usage_records` failed over the range holding the withdrawn \
                 pair, so the exclusion could not be decided: {err}"
            )
        })?;
    let count = result.buckets.len();
    let mut buckets = result.buckets.into_iter();
    match (buckets.next(), buckets.next()) {
        (Some(bucket), None) => Ok(bucket.value),
        _ => Err(format!(
            "`query_aggregated_usage_records` was dispatched with no grouping dimension, which \
             the aggregate surface fixes as the no-grouping case - a single bucket carrying an \
             empty key - and it answered {count} buckets. There is no one total to compare the \
             live entry's quantity against."
        )),
    }
}

/// Every entry the range under test comes back with on the raw path.
///
/// A `Vec` rather than a set, because the count is an assertion: a set
/// would collapse a duplicated row away.
async fn fold_page(plugin: &dyn UsageCollectorPluginV1) -> Result<Vec<UsageRecord>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(FOLD_WINDOW_FROM, FOLD_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let page = plugin
        .list_usage_records(meter, range, &contract_query(FOLD_PAGE_LIMIT), &[])
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range holding the withdrawn pair, so \
                 whether both its entries stay readable could not be decided: {err}"
            )
        })?;
    Ok(page.items)
}

/// Builds the live entry, the entry withdrawn from under it, and the
/// invalidation that withdraws it.
///
/// Two guards keep the check from passing by construction, and both are the
/// suite's own facts rather than the plugin's:
///
/// * The two quantities differ. If they did not, a fold that counted the
///   withdrawn pair would report the same total as one that excluded it.
/// * The three entries derive three distinct ids. The ledger half counts
///   rows under them, and two fixtures sharing an id would collapse into an
///   idempotent replay rather than into two entries.
fn fold_fixtures() -> Result<FoldFixtures, String> {
    let live_value = Decimal::from_str(FOLD_LIVE_QUANTITY).map_err(|err| {
        format!("the check's own live quantity `{FOLD_LIVE_QUANTITY}` does not parse: {err}")
    })?;
    let withdrawn_value = Decimal::from_str(FOLD_WITHDRAWN_QUANTITY).map_err(|err| {
        format!(
            "the check's own withdrawn quantity `{FOLD_WITHDRAWN_QUANTITY}` does not parse: {err}"
        )
    })?;
    if live_value == withdrawn_value {
        return Err(format!(
            "the live and the withdrawn quantity are both `{FOLD_LIVE_QUANTITY}`, so a fold \
             counting the withdrawn pair would report the same total as one excluding it and this \
             check would pass by construction"
        ));
    }
    let expected_total = BigDecimal::from_str(FOLD_LIVE_QUANTITY).map_err(|err| {
        format!(
            "the check's own live quantity `{FOLD_LIVE_QUANTITY}` does not widen to the aggregate \
             carrier: {err}"
        )
    })?;

    let live_end = FOLD_WINDOW_FROM.saturating_add(time::Duration::hours(1));
    let pair_end = FOLD_WINDOW_FROM.saturating_add(time::Duration::hours(2));

    let live = fixture_record(&fold_key("live")?, live_value, FOLD_WINDOW_FROM, live_end)?;
    let withdrawn = fixture_record(&fold_key("withdrawn")?, withdrawn_value, live_end, pair_end)?;
    // The invalidation carries its target's covered period, which is the
    // shape the gateway admits and the shape the SPI reasons about: both
    // entries carry one covered period, so no `time_range` selects one of
    // the pair without the other and no placement of the invalidation
    // changes a result. It carries the target's quantity too, echoed
    // rather than negated.
    let invalidation = fixture_invalidation(
        &fold_key("invalidation")?,
        withdrawn_value,
        live_end,
        pair_end,
        withdrawn.id,
    )?;

    let ids = BTreeSet::from([live.id, withdrawn.id, invalidation.id]);
    if ids.len() != 3 {
        return Err(format!(
            "the live entry ({live}), the withdrawn entry ({withdrawn}) and the invalidation \
             ({invalidation}) do not derive three distinct ids, so two of them would collapse \
             into an idempotent replay and the row count this check asserts would be meaningless",
            live = live.id,
            withdrawn = withdrawn.id,
            invalidation = invalidation.id,
        ));
    }

    Ok(FoldFixtures {
        live,
        withdrawn,
        invalidation,
        expected_total,
    })
}

/// The idempotency key one role of this check's fixture submits under.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture)
/// spells out: in a ledger with no delete path, an edited fixture must take
/// a fresh identity rather than inherit an accepted entry's.
fn fold_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{INVALIDATION_EXCLUDED_FROM_FOLD}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}
