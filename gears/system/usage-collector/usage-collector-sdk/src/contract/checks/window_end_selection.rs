//! The DESIGN §3.3 `window-end-selection` check.
//!
//! See [`window_end_selection`] for what it asserts; the module holds the
//! rows it probes the period rule from and the single read that decides
//! them.

use std::collections::BTreeSet;

use rust_decimal::Decimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_METER_TYPE_ID, FIXTURE_EPOCH, contract_query, fixture_record, violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, WINDOW_END_SELECTION};
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

/// The inclusive lower bound of the range `window_end_selection` reads
/// over, and the instant its entries are arranged around.
///
/// Thirty days past [`FIXTURE_EPOCH`], which is what keeps this check's
/// entries out of every other check's read range and every other check's
/// entries out of this one's. Every fixture in the suite shares one meter
/// and one tenant — deliberately, so that nothing but the covered period
/// can decide any of these entries — which leaves the period as the only
/// thing separating one check's entries from another's. It matters more
/// here than elsewhere because this check asserts that some entries are
/// **not** returned, and a stray entry inside the range would be read as
/// one of them.
pub const WINDOW_SELECTION_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(30));

/// The exclusive upper bound of that range: one hour past
/// [`WINDOW_SELECTION_FROM`].
const WINDOW_SELECTION_TO: time::OffsetDateTime =
    WINDOW_SELECTION_FROM.saturating_add(time::Duration::hours(1));

/// One side of the period rule: `(case, window_start, window_end, selected,
/// why)`, with both bounds in minutes from [`WINDOW_SELECTION_FROM`].
type SelectionCase = (&'static str, i64, i64, bool, &'static str);

/// The sides of `from <= window_end < to` this check reads it from.
///
/// The range under test is `[WINDOW_SELECTION_FROM, WINDOW_SELECTION_TO)`,
/// so `0` is `from` and `60` is `to`. Each row is asserted on its own; see
/// [`window_end_selection`] for why they are one check and not five.
const SELECTION_CASES: &[SelectionCase] = &[
    (
        "selected-by-its-end",
        -60,
        30,
        true,
        "Its `window_end` falls inside the range while its `window_start` precedes `from`, so \
         the entry is selected by its end and by nothing else. A backend that filters on \
         `window_start`, the shape a port of the old point-in-time column takes, misses it.",
    ),
    (
        "end-at-the-upper-bound",
        30,
        60,
        false,
        "Its `window_end` is exactly `to`, and the upper bound is exclusive. A `BETWEEN` \
         inclusive at both ends selects it, and two adjacent ranges would then both count it.",
    ),
    (
        "end-at-the-lower-bound",
        -30,
        0,
        true,
        "Its `window_end` is exactly `from`, and the lower bound is inclusive. A range exclusive \
         at both ends drops it, and no range abutting this one would pick it up either.",
    ),
    (
        "wider-than-the-range",
        -120,
        120,
        false,
        "Its period strictly contains the range, so period-end selection leaves it out at both \
         ends. An interval-overlap predicate (`window_start < to && window_end > from`) selects \
         it instead.",
    ),
    (
        "point-event",
        45,
        45,
        true,
        "`window_start == window_end` inside the range, selected by the same \
         `from <= window_end < to` as every other entry. A backend carrying a special arm for a \
         zero-length period drops it.",
    ),
];

/// One entry `window_end_selection` submits, and whether the range under
/// test must return it.
struct SelectionFixture {
    /// The side of the rule this entry probes. Names the entry in a
    /// violation detail and, through its idempotency key, in the ledger.
    case: &'static str,
    /// The entry as submitted.
    record: UsageRecord,
    /// Whether `[from, to)` must select it.
    selected: bool,
    /// Why it must, and which plausible backend answers otherwise.
    why: &'static str,
}

/// `window-end-selection` — *"A range selects by period end, exclusive at
/// the upper bound. A point event needs no special case. An entry wider
/// than the range is selected by neither side."*
///
/// One entry per row of [`SELECTION_CASES`] is submitted, and one
/// `list_usage_records` over `[from, to)` decides all of them at once.
/// Every entry shares the suite's meter, tenant and resource, so nothing
/// but its covered period can decide it.
///
/// **Each row is asserted separately.** A single "the boundary behaves"
/// assertion that OR-ed the two bound rows together would pass against a
/// backend that got both wrong in compensating directions — an inclusive
/// upper bound and an exclusive lower one keep the row count and move the
/// window, which is exactly the failure this check exists to name.
///
/// They are one check because they are one rule —
/// `cpt-cf-usage-collector-adr-window-end-selection`, which
/// [`TimeRange::contains_window_end`] spells once — read from the sides
/// that catch different plausible backends. Selection on `window_start`
/// fails the first row; an off-by-one in a `BETWEEN` fails one of the two
/// bound rows and an exclusive lower bound fails the other; an
/// interval-overlap predicate, the natural thing to write against a period
/// and the wrong thing here, fails the wider-than-the-range row. The
/// point-event row is the rule's own claim that it needs no special arm:
/// the same predicate that selects an hour-long period selects a
/// zero-length one, so a backend has nothing to case-split on.
pub async fn window_end_selection(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let (fixtures, mut violations) = selection_fixtures();

    let mut admitted = Vec::new();
    for fixture in &fixtures {
        if let Err(err) = plugin.create_usage_record(fixture.record.clone()).await {
            violations.push(violation(
                WINDOW_END_SELECTION,
                format!(
                    "`create_usage_record` refused the `{case}` entry (record {id}), so the side \
                     of period-end selection it stands for was never asserted: {err}",
                    case = fixture.case,
                    id = fixture.record.id,
                ),
            ));
            continue;
        }
        admitted.push(fixture);
    }

    let returned = match selection_page(plugin, fixtures.len()).await {
        Ok(returned) => returned,
        Err(detail) => {
            violations.push(violation(WINDOW_END_SELECTION, detail));
            return violations;
        }
    };

    for fixture in admitted {
        if returned.contains(&fixture.record.id) == fixture.selected {
            continue;
        }
        let (required, observed) = if fixture.selected {
            ("must select", "did not return it")
        } else {
            ("must not select", "returned it")
        };
        violations.push(violation(
            WINDOW_END_SELECTION,
            format!(
                "the range `[{from}, {to})` {required} the `{case}` entry (record {id}, covered \
                 period `{start}` to `{end}`), and `list_usage_records` {observed}. {why}",
                from = WINDOW_SELECTION_FROM,
                to = WINDOW_SELECTION_TO,
                case = fixture.case,
                id = fixture.record.id,
                start = fixture.record.window_start,
                end = fixture.record.window_end,
                why = fixture.why,
            ),
        ));
    }
    violations
}

/// One fixture per row of [`SELECTION_CASES`], alongside a harness fault
/// per row that could not be built.
///
/// Partitioned rather than short-circuited, for the reason
/// [`quantity_fixtures`](super::quantity_round_trip::quantity_fixtures)
/// gives: one unbuildable row must not suppress the other four, and the
/// suite's own failure must not be reported under a DESIGN check name.
fn selection_fixtures() -> (Vec<SelectionFixture>, Vec<ContractViolation>) {
    let mut built = Vec::new();
    let mut faults = Vec::new();
    for &(case, start, end, selected, why) in SELECTION_CASES {
        match selection_fixture(case, start, end) {
            Ok(record) => built.push(SelectionFixture {
                case,
                record,
                selected,
                why,
            }),
            Err(detail) => faults.push(violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{case}` fixture, so that side \
                     of period-end selection was never asserted. This is a fault in the suite, \
                     not in the plugin under test: {detail}"
                ),
            )),
        }
    }
    (built, faults)
}

/// Builds the entry for one row, offsetting both bounds from
/// [`WINDOW_SELECTION_FROM`].
///
/// The idempotency key carries the case name, so a row that is edited,
/// reordered or inserted takes a fresh identity rather than inheriting the
/// id of whatever used to sit beside it — the hazard
/// [`quantity_fixture`](super::quantity_round_trip::quantity_fixture)
/// spells out at length, in a ledger with no delete path.
fn selection_fixture(case: &str, start: i64, end: i64) -> Result<UsageRecord, String> {
    let idempotency_key = IdempotencyKey::new(format!("{WINDOW_END_SELECTION}-{case}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
    fixture_record(
        &idempotency_key,
        Decimal::ONE,
        WINDOW_SELECTION_FROM.saturating_add(time::Duration::minutes(start)),
        WINDOW_SELECTION_FROM.saturating_add(time::Duration::minutes(end)),
    )
}

/// Every `UsageRecord.id` the range under test comes back with.
///
/// The limit is the fixture count rather than the count this check expects
/// to be selected. Only these entries lie anywhere near the range, so no
/// correct or incorrect backend can return more of them than were
/// submitted — while a limit set to the expected selection would truncate a
/// wrongly selected entry away and turn a violation into a pass.
async fn selection_page(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: usize,
) -> Result<BTreeSet<Uuid>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(WINDOW_SELECTION_FROM, WINDOW_SELECTION_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let limit = u64::try_from(fixtures).unwrap_or(u64::MAX);
    let page = plugin
        .list_usage_records(meter, range, &contract_query(limit), &[])
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range under test, so no side of period-end \
                 selection could be decided: {err}"
            )
        })?;
    Ok(page.items.into_iter().map(|item| item.id).collect())
}
