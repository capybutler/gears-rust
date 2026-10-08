//! The DESIGN §3.3 `reconciliation-figures` check.
//!
//! See [`reconciliation_figures`] for what it asserts. One function per
//! case, each on its own meter under [`check_meter`] so no assertion can be
//! decided by another's entries. Every case reads over
//! [`FIGURES_WINDOW_FROM`] .. [`FIGURES_WINDOW_TO`], the offset
//! [`check_window_from`] tables for this check.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, CONTRACT_TENANT_ID, SCOPE_EXCLUDED_TENANT_ID, check_meter,
    check_window_from, contract_scope, fixture_invalidation, fixture_record_on, seed_usage_record,
    violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, RECONCILIATION_FIGURES};
use crate::models::{AggregationFold, IdempotencyKey};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::reconciliation::{QuantitySummary, ReconciliationMetadata};
use crate::stored::{MeterRef, StoredUsageRecord};
use crate::time_range::TimeRange;

/// What [`accrues_fixtures`] builds: the meter, the read range, the three
/// entries, and the accrual the two in-range ones must produce.
type AccruesFixtures = (MeterRef, TimeRange, [StoredUsageRecord; 3], BigDecimal);

/// What [`observes_fixtures`] builds: the meter, the read range, the three
/// entries, and the quantity the entry with the greatest `window_end`
/// carries.
type ObservesFixtures = (MeterRef, TimeRange, [StoredUsageRecord; 3], UsageQuantity);

/// What [`withdrawn_fixtures`] builds: the meter, the read range, and the
/// record-then-invalidation pair.
type WithdrawnFixtures = (MeterRef, TimeRange, [StoredUsageRecord; 2]);

/// What [`empty_range_fixtures`] builds: the meter, the read range, the two
/// out-of-range entries, and the `window_end` the later of them carries.
type EmptyRangeFixtures = (
    MeterRef,
    TimeRange,
    [StoredUsageRecord; 2],
    time::OffsetDateTime,
);

/// What [`scope_excluded_fixtures`] builds: the meter, the read range, and
/// the two entries under [`SCOPE_EXCLUDED_TENANT_ID`].
type ScopeExcludedFixtures = (MeterRef, TimeRange, [StoredUsageRecord; 2]);

/// The inclusive lower bound of the range every case below reads over.
///
/// The offset [`check_window_from`] tables for this check, which keeps its
/// entries and every other check's out of each other's ranges. It matters
/// more here than in most: case 4 asserts an entry is **not** selected, and a
/// stray entry inside the range would read as one of the range's own.
const FIGURES_WINDOW_FROM: time::OffsetDateTime = check_window_from(RECONCILIATION_FIGURES, "main");

/// The exclusive upper bound of that range: one hour past
/// [`FIGURES_WINDOW_FROM`].
const FIGURES_WINDOW_TO: time::OffsetDateTime =
    FIGURES_WINDOW_FROM.saturating_add(time::Duration::hours(1));

/// The read range every case below dispatches.
fn figures_range() -> Result<TimeRange, String> {
    TimeRange::new(FIGURES_WINDOW_FROM, FIGURES_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))
}

/// This check's idempotency key for `role`.
///
/// Keyed on the role name for the reason
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture) gives:
/// in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's.
fn figures_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{RECONCILIATION_FIGURES}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}

/// Builds one fixture entry on `meter`, attributed to `tenant_id`, with
/// `window_end` offset from [`FIGURES_WINDOW_FROM`] by `window_end_minutes`
/// and `accepted_at` offset from [`CONTRACT_ACCEPTED_AT`] by
/// `accepted_at_minutes`.
///
/// `window_start` is one minute before `window_end` rather than pinned to
/// [`FIGURES_WINDOW_FROM`] for every entry, so a subject reading
/// `window_start` instead (`Defect::SelectsOnWindowStart`) cannot agree with
/// a conforming backend by accident.
fn entry(
    meter: &MeterRef,
    tenant_id: Uuid,
    role: &str,
    quantity: &str,
    accepted_at_minutes: i64,
    window_end_minutes: i64,
) -> Result<StoredUsageRecord, String> {
    let key = figures_key(role)?;
    let parsed = UsageQuantity::parse(quantity)
        .map_err(|err| format!("the check's own quantity `{quantity}` does not parse: {err}"))?;
    let window_end =
        FIGURES_WINDOW_FROM.saturating_add(time::Duration::minutes(window_end_minutes));
    fixture_record_on(
        meter,
        tenant_id,
        &key,
        parsed,
        CONTRACT_ACCEPTED_AT.saturating_add(time::Duration::minutes(accepted_at_minutes)),
        window_end.saturating_sub(time::Duration::minutes(1)),
        window_end,
    )
}

/// Submits `records`, reporting any refusal against this check rather than
/// against [`HARNESS_FAULT`]: a submission refused by the plugin under test
/// is the plugin's answer, not the suite's failure to build a scenario.
async fn submit(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterRef,
    records: &[StoredUsageRecord],
    role: &str,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for record in records {
        if let Err(err) = seed_usage_record(plugin, meter, record.clone()).await {
            violations.push(violation(
                RECONCILIATION_FIGURES,
                format!(
                    "`create_usage_records` refused the `{role}` case's entry (record {id}), so \
                     that case's reconciliation figures were never asserted: {err}",
                    id = record.id,
                ),
            ));
        }
    }
    violations
}

/// A fixture the suite itself could not build, reported as [`HARNESS_FAULT`]
/// rather than against the plugin.
fn harness_fault(role: &str, detail: &str) -> ContractViolation {
    violation(
        HARNESS_FAULT,
        format!(
            "the contract suite could not build its own `{role}` fixtures for \
             `{RECONCILIATION_FIGURES}`, so that case was never asserted. This is a fault in \
             the suite, not in the plugin under test: {detail}"
        ),
    )
}

/// `reconciliation-figures` — the DESIGN §3.3 row over
/// [`UsageCollectorPluginV1::get_reconciliation_metadata`].
///
/// Reconciliation is the one SPI read path whose obligations no other check
/// reaches:
/// [`scope_is_a_filter_on_every_read_path`](super::scope_is_a_filter_on_every_read_path())
/// names why it reaches neither this method nor the feed page, and the feed
/// page is covered incidentally by two feed checks.
///
/// Each assertion sits on its own meter, so none can be decided by another's
/// entries, and each is named in the violation it raises:
///
/// 1. `accrues` — a `SUM` meter reports the `Accrued` branch, and the
///    accrual covers exactly the entries the range selects.
/// 2. `observes` — a `MAX` meter reports the `Observations` branch, with
///    `count` equal to the number of entries the range selects and `latest`
///    the entry with the greatest `window_end` — **not** the greatest
///    quantity.
/// 3. `withdrawn` — a record and its invalidation both keep `accepted_count`
///    at two, and the summary is empty for the declared fold.
/// 4. `empty-range` — two entries outside the range leave `accepted_count`
///    at zero and the summary empty, and still populate both watermarks:
///    they are unbounded by the range.
/// 5. `no-entries` — a meter nothing was ever written to answers exactly
///    [`ReconciliationMetadata::empty_for`], watermarks included.
/// 6. `excluded` — a tenant the compiled scope excludes answers
///    field-for-field identical to case 5, never an error.
///
/// **The fold selects the summary's branch and never its ordering.** Case 2
/// deliberately gives its three entries a `window_end` order that disagrees
/// with their quantity order, so a backend that reached for its own `MAX`
/// fold here — reporting the greatest *quantity* rather than the entry with
/// the greatest `window_end` — answers wrongly.
pub async fn reconciliation_figures(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    violations.extend(case_accrues(plugin).await);
    violations.extend(case_observes(plugin).await);
    violations.extend(case_withdrawn(plugin).await);
    violations.extend(case_empty_range(plugin).await);
    violations.extend(case_no_entries(plugin).await);
    violations.extend(case_scope_excluded(plugin).await);
    violations
}

/// Case 1, `accrues`.
///
/// **Oracle**: a backend folding every entry ever written to this meter
/// rather than only the ones the range selects reports a total heavier by the
/// out-of-range entry's quantity, and the detail names both figures. One
/// reporting the `Observations` branch for an accruing fold
/// (`Defect::ReconciliationSummaryIgnoresWhetherTheFoldAccrues`) is caught by
/// the branch check below.
async fn case_accrues(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    const ROLE: &str = "accrues";
    const IN_RANGE_A: &str = "2.50";
    const IN_RANGE_B: &str = "3.75";
    const OUT_OF_RANGE: &str = "999.00";

    let built = accrues_fixtures();
    let (meter, range, records, expected_total) = match built {
        Ok(built) => built,
        Err(detail) => return vec![harness_fault(ROLE, &detail)],
    };

    let mut violations = submit(plugin, &meter, &records, ROLE).await;
    if !violations.is_empty() {
        return violations;
    }

    let answer = match plugin
        .get_reconciliation_metadata(
            CONTRACT_TENANT_ID,
            &meter,
            range,
            AggregationFold::Sum,
            &contract_scope(),
        )
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            violations.push(violation(
                RECONCILIATION_FIGURES,
                format!(
                    "case `{ROLE}`: `get_reconciliation_metadata` failed over a `SUM` meter \
                     holding two in-range entries and one out-of-range one: {err}. The SPI \
                     promises an ordinary answer here, never an error."
                ),
            ));
            return violations;
        }
    };

    let QuantitySummary::Accrued(total) = &answer.quantity_summary else {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: a `SUM` fold must report the `Accrued` branch of \
                 `QuantitySummary` — `QuantitySummary::accrues` is `true` only for `Sum` — and \
                 this backend answered `{summary:?}` instead. A backend that reports the \
                 `Observations` branch for an accruing fold has the two branches swapped or \
                 keyed on something other than the declared fold.",
                summary = answer.quantity_summary,
            ),
        ));
        return violations;
    };

    if *total != expected_total {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: the accrual over `[{FIGURES_WINDOW_FROM}, {FIGURES_WINDOW_TO})` \
                 must be `{expected_total}` — the sum of the two entries whose `window_end` \
                 falls inside the range (`{IN_RANGE_A}` and `{IN_RANGE_B}`) — and this backend \
                 answered `{total}`. A third entry of `{OUT_OF_RANGE}` covers a period ending \
                 outside the range; a backend that accrues over every entry this meter ever held \
                 rather than only the range's selection answers a total heavier by that amount.",
            ),
        ));
    }
    violations
}

/// The fixtures [`case_accrues`] submits: the meter, the read range, the
/// three entries, and the accrual the two in-range ones must produce.
fn accrues_fixtures() -> Result<AccruesFixtures, String> {
    let meter = check_meter(RECONCILIATION_FIGURES, "accrues")?;
    let range = figures_range()?;
    let a = entry(&meter, CONTRACT_TENANT_ID, "accrues-a", "2.50", 0, 10)?;
    let b = entry(&meter, CONTRACT_TENANT_ID, "accrues-b", "3.75", 0, 20)?;
    let outside = entry(
        &meter,
        CONTRACT_TENANT_ID,
        "accrues-outside",
        "999.00",
        0,
        90,
    )?;
    let a_value = BigDecimal::from_str("2.50")
        .map_err(|err| format!("the check's own quantity `2.50` does not widen: {err}"))?;
    let b_value = BigDecimal::from_str("3.75")
        .map_err(|err| format!("the check's own quantity `3.75` does not widen: {err}"))?;
    Ok((meter, range, [a, b, outside], a_value + b_value))
}

/// Case 2, `observes`.
///
/// **Oracle**: the entries' `window_end` order is the reverse of their
/// quantity order, so a backend answering the greatest *quantity* rather than
/// the entry with the greatest `window_end` reports a different `latest`. It
/// is also the only test exercising the reference's own `latest_observation`
/// tie-break with more than one surviving candidate row.
async fn case_observes(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    const ROLE: &str = "observes";

    let built = observes_fixtures();
    let (meter, range, records, expected_latest) = match built {
        Ok(built) => built,
        Err(detail) => return vec![harness_fault(ROLE, &detail)],
    };

    let mut violations = submit(plugin, &meter, &records, ROLE).await;
    if !violations.is_empty() {
        return violations;
    }

    let answer = match plugin
        .get_reconciliation_metadata(
            CONTRACT_TENANT_ID,
            &meter,
            range,
            AggregationFold::Max,
            &contract_scope(),
        )
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            violations.push(violation(
                RECONCILIATION_FIGURES,
                format!(
                    "case `{ROLE}`: `get_reconciliation_metadata` failed over a `MAX` meter \
                     holding three in-range entries: {err}. The SPI promises an ordinary answer \
                     here, never an error."
                ),
            ));
            return violations;
        }
    };

    let QuantitySummary::Observations(observed) = answer.quantity_summary else {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: a `MAX` fold must report the `Observations` branch of \
                 `QuantitySummary` — `QuantitySummary::accrues` is `false` for every fold but \
                 `Sum` — and this backend answered `{summary:?}` instead.",
                summary = answer.quantity_summary,
            ),
        ));
        return violations;
    };
    let count = observed.as_ref().map_or(0, |o| o.count.get());
    let latest = observed.map(|o| o.latest);

    if count != 3 {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: three entries fall inside \
                 `[{FIGURES_WINDOW_FROM}, {FIGURES_WINDOW_TO})` and none is withdrawn, so \
                 `count` must be `3`; this backend answered `{count}`.",
            ),
        ));
    }
    if latest != Some(expected_latest) {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: `latest` must be the entry with the greatest `window_end` \
                 (quantity `{expected_latest}`), not the entry with the greatest quantity; this \
                 backend answered `{latest:?}`. The three entries' `window_end` order is the \
                 reverse of their quantity order precisely so a backend that reaches for its own \
                 `MAX` fold here, rather than the §3.1 `LATEST` order, is caught.",
            ),
        ));
    }
    violations
}

/// The fixtures [`case_observes`] submits: the meter, the read range, the
/// three entries, and the quantity the entry with the greatest `window_end`
/// carries.
///
/// Guarded: the entry with the greatest `window_end` must not also carry the
/// greatest quantity, or a backend answering the greatest quantity passes by
/// construction.
fn observes_fixtures() -> Result<ObservesFixtures, String> {
    let meter = check_meter(RECONCILIATION_FIGURES, "observes")?;
    let range = figures_range()?;
    // `accepted_at` rises with `window_end` deliberately. The entries carry
    // distinct `window_end` values, so DESIGN §3.1's `LATEST` order picks the
    // winner on that key alone; keeping `accepted_at` monotonic with it means
    // a subject dropping `window_end` from the order
    // (`Defect::LatestIgnoresThePeriodEnd`) still picks the same winner, so
    // this case does not start discriminating a tie-break rule
    // `latest-tie-break` already owns.
    let greatest_quantity = entry(&meter, CONTRACT_TENANT_ID, "observes-a", "50", 0, 10)?;
    let middle = entry(&meter, CONTRACT_TENANT_ID, "observes-b", "10", 1, 20)?;
    let greatest_window_end = entry(&meter, CONTRACT_TENANT_ID, "observes-c", "5", 2, 30)?;

    if greatest_window_end.quantity.as_decimal() >= greatest_quantity.quantity.as_decimal() {
        return Err(
            "the entry with the greatest `window_end` must carry a smaller quantity than the \
             entry with the greatest quantity, or a backend that answered the greatest quantity \
             would pass this case by construction"
                .to_owned(),
        );
    }

    let expected_latest = greatest_window_end.quantity;
    Ok((
        meter,
        range,
        [greatest_quantity, middle, greatest_window_end],
        expected_latest,
    ))
}

/// Case 3, `withdrawn`.
///
/// **Oracle**: `accepted_count` and the quantity summary read the same range
/// and deliberately disagree. A backend that lets a withdrawal reduce
/// `accepted_count`, or that includes a withdrawn pair in the summary,
/// answers a figure that disagrees with the one asserted here.
async fn case_withdrawn(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    const ROLE: &str = "withdrawn";

    let built = withdrawn_fixtures();
    let (meter, range, records) = match built {
        Ok(built) => built,
        Err(detail) => return vec![harness_fault(ROLE, &detail)],
    };

    let mut violations = submit(plugin, &meter, &records, ROLE).await;
    if !violations.is_empty() {
        return violations;
    }

    let fold = AggregationFold::Count;
    let answer = match plugin
        .get_reconciliation_metadata(CONTRACT_TENANT_ID, &meter, range, fold, &contract_scope())
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            violations.push(violation(
                RECONCILIATION_FIGURES,
                format!(
                    "case `{ROLE}`: `get_reconciliation_metadata` failed over a meter holding a \
                     withdrawn pair: {err}. The SPI promises an ordinary answer here, never an \
                     error."
                ),
            ));
            return violations;
        }
    };

    if answer.accepted_count != 2 {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: a record and the invalidation that withdraws it are two accepted \
                 entries, and `accepted_count` reports ingestion activity rather than the meter — \
                 it must be `2` whether or not the pair is withdrawn. This backend answered \
                 `{observed}`.",
                observed = answer.accepted_count,
            ),
        ));
    }
    let expected_summary = QuantitySummary::empty_for(fold);
    if answer.quantity_summary != expected_summary {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: the summary excludes both halves of a withdrawn pair, so over a \
                 meter holding nothing else it must be `{expected_summary:?}`; this backend \
                 answered `{observed:?}`.",
                observed = answer.quantity_summary,
            ),
        ));
    }
    violations
}

/// The fixtures [`case_withdrawn`] submits: the meter, the read range, and
/// the record-then-invalidation pair, both inside the range.
fn withdrawn_fixtures() -> Result<WithdrawnFixtures, String> {
    let meter = check_meter(RECONCILIATION_FIGURES, "withdrawn")?;
    let range = figures_range()?;
    let record = entry(
        &meter,
        CONTRACT_TENANT_ID,
        "withdrawn-record",
        "4.00",
        0,
        10,
    )?;
    let invalidation = fixture_invalidation(&meter, &record)?;
    Ok((meter, range, [record, invalidation]))
}

/// Case 4, `empty-range`.
///
/// **Oracle**: both entries lie outside the range, so a conforming backend
/// answers zero activity and an empty summary — and still names both
/// watermarks, which are unbounded by the range. One reading the watermarks
/// from the range's own selection answers `None` where this requires `Some`.
async fn case_empty_range(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    const ROLE: &str = "empty-range";

    let built = empty_range_fixtures();
    let (meter, range, records, expected_max_window_end) = match built {
        Ok(built) => built,
        Err(detail) => return vec![harness_fault(ROLE, &detail)],
    };

    let mut violations = submit(plugin, &meter, &records, ROLE).await;
    if !violations.is_empty() {
        return violations;
    }

    let fold = AggregationFold::Sum;
    let answer = match plugin
        .get_reconciliation_metadata(CONTRACT_TENANT_ID, &meter, range, fold, &contract_scope())
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            violations.push(violation(
                RECONCILIATION_FIGURES,
                format!(
                    "case `{ROLE}`: `get_reconciliation_metadata` failed over a meter holding \
                     only out-of-range entries: {err}. The SPI promises an ordinary answer here, \
                     never an error."
                ),
            ));
            return violations;
        }
    };

    if answer.accepted_count != 0 {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: both entries this meter holds cover a period ending outside \
                 `[{FIGURES_WINDOW_FROM}, {FIGURES_WINDOW_TO})`, so `accepted_count` over that \
                 range must be `0`; this backend answered `{observed}`.",
                observed = answer.accepted_count,
            ),
        ));
    }
    let expected_summary = QuantitySummary::empty_for(fold);
    if answer.quantity_summary != expected_summary {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: the range selects nothing, so the summary must be \
                 `{expected_summary:?}`; this backend answered `{observed:?}`.",
                observed = answer.quantity_summary,
            ),
        ));
    }
    if answer.max_accepted_at != Some(CONTRACT_ACCEPTED_AT) {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: both watermarks are unbounded by the requested range and absent \
                 only on a scope holding no entries, so a range selecting nothing must still \
                 report `max_accepted_at` as `Some({CONTRACT_ACCEPTED_AT})`; this backend \
                 answered `{observed:?}`. A backend that derives the watermarks from the range's \
                 own selection rather than from the scope answers `None` here instead.",
                observed = answer.max_accepted_at,
            ),
        ));
    }
    if answer.max_window_end != Some(expected_max_window_end) {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: `max_window_end` is unbounded by the requested range, so it must \
                 be `Some({expected_max_window_end})` — the greater of the two out-of-range \
                 entries' covered-period ends; this backend answered `{observed:?}`.",
                observed = answer.max_window_end,
            ),
        ));
    }
    violations
}

/// The fixtures [`case_empty_range`] submits: the meter, the read range, the
/// two out-of-range entries, and the `window_end` the later of them carries.
fn empty_range_fixtures() -> Result<EmptyRangeFixtures, String> {
    let meter = check_meter(RECONCILIATION_FIGURES, "empty-range")?;
    let range = figures_range()?;
    let earlier = entry(&meter, CONTRACT_TENANT_ID, "empty-range-a", "1.00", 0, 90)?;
    let later = entry(&meter, CONTRACT_TENANT_ID, "empty-range-b", "2.00", 0, 100)?;
    if earlier.window_end >= FIGURES_WINDOW_TO && later.window_end >= FIGURES_WINDOW_TO {
        let max_window_end = later.window_end;
        return Ok((meter, range, [earlier, later], max_window_end));
    }
    Err(format!(
        "this check's `empty-range` entries must both cover a period ending at or after \
         `{FIGURES_WINDOW_TO}`, or the case would assert `accepted_count == 0` against a range \
         that actually selects one of them"
    ))
}

/// Case 5, `no-entries`.
///
/// **Oracle**: a meter nothing was ever written to must answer exactly
/// [`ReconciliationMetadata::empty_for`]. A backend that reports a watermark
/// for an unwritten scope, or the wrong summary branch for the declared
/// fold, disagrees with the one value this case allows.
async fn case_no_entries(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    const ROLE: &str = "no-entries";

    let meter = match check_meter(RECONCILIATION_FIGURES, ROLE) {
        Ok(meter) => meter,
        Err(detail) => return vec![harness_fault(ROLE, &detail)],
    };
    let range = match figures_range() {
        Ok(range) => range,
        Err(detail) => return vec![harness_fault(ROLE, &detail)],
    };

    let fold = AggregationFold::Max;
    let answer = match plugin
        .get_reconciliation_metadata(CONTRACT_TENANT_ID, &meter, range, fold, &contract_scope())
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            return vec![violation(
                RECONCILIATION_FIGURES,
                format!(
                    "case `{ROLE}`: `get_reconciliation_metadata` failed over a meter this check \
                     never wrote to: {err}. The SPI promises an ordinary answer here, never an \
                     error."
                ),
            )];
        }
    };

    let expected = ReconciliationMetadata::empty_for(fold);
    if answer == expected {
        return Vec::new();
    }
    vec![violation(
        RECONCILIATION_FIGURES,
        format!(
            "case `{ROLE}`: a meter this check never wrote to must answer exactly \
             `{expected:?}`; this backend answered `{answer:?}`.",
        ),
    )]
}

/// Case 6, `excluded`.
///
/// **Oracle**: two entries are stored inside the read range under a tenant
/// the compiled scope excludes, so the case cannot pass merely because
/// nothing was written. The SPI doc names this as unreached by
/// `scope_is_a_filter_on_every_read_path`: *"A tenant the compiled scope
/// excludes answers exactly as one holding no entries."* A backend answering
/// an error, or folding these entries in because `tenant_id` matches the
/// call's own parameter, disagrees with the required value.
async fn case_scope_excluded(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    const ROLE: &str = "excluded";

    let built = scope_excluded_fixtures();
    let (meter, range, records) = match built {
        Ok(built) => built,
        Err(detail) => return vec![harness_fault(ROLE, &detail)],
    };

    let mut violations = submit(plugin, &meter, &records, ROLE).await;
    if !violations.is_empty() {
        return violations;
    }

    let fold = AggregationFold::Max;
    let answer = match plugin
        .get_reconciliation_metadata(
            SCOPE_EXCLUDED_TENANT_ID,
            &meter,
            range,
            fold,
            &contract_scope(),
        )
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            violations.push(violation(
                RECONCILIATION_FIGURES,
                format!(
                    "case `{ROLE}`: `get_reconciliation_metadata` answered an error ({err}) for \
                     a tenant the compiled scope excludes. The SPI requires that tenant to \
                     answer exactly as one holding no entries, never an error."
                ),
            ));
            return violations;
        }
    };

    let expected = ReconciliationMetadata::empty_for(fold);
    if answer != expected {
        violations.push(violation(
            RECONCILIATION_FIGURES,
            format!(
                "case `{ROLE}`: two entries were stored under a tenant the compiled scope \
                 excludes, inside the read range. The compiled scope applies first, so this \
                 tenant must answer exactly `{expected:?}` — field-for-field identical to a \
                 tenant holding no entries at all; this backend answered `{answer:?}`. A backend \
                 that decided this call from the `tenant_id` parameter rather than the compiled \
                 `scope` would fold these entries in instead.",
            ),
        ));
    }
    violations
}

/// The fixtures [`case_scope_excluded`] submits: the meter, the read range,
/// and the two entries, both under [`SCOPE_EXCLUDED_TENANT_ID`] and both
/// inside the range.
fn scope_excluded_fixtures() -> Result<ScopeExcludedFixtures, String> {
    let meter = check_meter(RECONCILIATION_FIGURES, "excluded")?;
    let range = figures_range()?;
    let a = entry(&meter, SCOPE_EXCLUDED_TENANT_ID, "excluded-a", "9", 0, 10)?;
    let b = entry(&meter, SCOPE_EXCLUDED_TENANT_ID, "excluded-b", "3", 0, 20)?;
    Ok((meter, range, [a, b]))
}
