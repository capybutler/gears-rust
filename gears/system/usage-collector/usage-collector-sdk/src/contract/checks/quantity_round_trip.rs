//! The DESIGN §3.3 `quantity-round-trip` check.
//!
//! See [`quantity_round_trip`] for what it asserts; the module holds that
//! check's fixtures, its read-back helper and nothing else.

use std::str::FromStr;

use rust_decimal::Decimal;
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_METER_TYPE_ID, FIXTURE_EPOCH, contract_query, fixture_record, violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, QUANTITY_ROUND_TRIP};
use crate::models::{IdempotencyKey, MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

/// The published quantity range, sampled at its corners.
///
/// `docs/usage-collector-v1.yaml:576-583` publishes the bound: *at most 28
/// significant decimal digits (leading zeros excluded) and at most 28
/// digits after the decimal point*, so the magnitude is below 10²⁸ and the
/// smallest non-zero value is 1×10⁻²⁸; *"Every storage plugin MUST
/// round-trip that full range — including its negative half — and the full
/// precision without loss"*.
///
/// The two clauses are jointly satisfiable by the carrier every surface
/// uses. `rust_decimal::Decimal` is a 96-bit mantissa with a scale of 0..=28,
/// so its ceiling is `79_228_162_514_264_337_593_543_950_335` (approximately 7.9e28) at
/// scale 0 — above the published 10²⁸ — and the two clauses never both bind
/// at once: a value carrying 28 fractional digits has at most 28
/// significant digits in total, so its integer part is zero and its
/// mantissa stays below 10²⁸. Every corner below is therefore representable
/// without approximation, and [`quantity_fixture`] re-checks that claim per
/// value rather than assuming it.
const PUBLISHED_RANGE_CORNERS: &[&str] = &[
    // Greatest magnitude the range admits: 28 significant digits, scale 0,
    // one below 10^28.
    "9999999999999999999999999999",
    // Its negation. The sign is never constrained, and the negative half is
    // called out in the published bound because a backend storing an
    // unsigned column would lose exactly this value.
    "-9999999999999999999999999999",
    // Smallest non-zero value: 1x10^-28, the full 28 fractional digits.
    "0.0000000000000000000000000001",
    // Its negation.
    "-0.0000000000000000000000000001",
    // Significant trailing zeros. Losing these is a *scale* failure rather
    // than a magnitude one, and the two fail independently: a backend
    // storing NUMERIC(38, 6) keeps this value's magnitude and drops the
    // corners above, and one normalising on write keeps every corner above
    // and drops this.
    "42.500",
];

/// `quantity-round-trip` — *"The full published range round-trips digit for
/// digit, negative half included."*
///
/// One entry per corner of [`PUBLISHED_RANGE_CORNERS`] is persisted through
/// `create_usage_record` and read back through `list_usage_records` over a
/// range containing its `window_end`, and the two renderings are compared.
///
/// **The comparison is on the rendered decimal, not on `==`.**
/// `Decimal`'s `PartialEq` compares numeric value, so it calls `42.5` and
/// `42.500` equal — and a backend that normalised the scale on write is
/// exactly one of the things this check exists to catch. Comparing
/// `to_string()` is what makes "digit for digit" mean what it says.
pub async fn quantity_round_trip(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let (fixtures, mut violations) = quantity_fixtures();
    for (literal, record) in fixtures {
        let id = record.id;
        let window_end = record.window_end;
        if let Err(err) = plugin.create_usage_record(record).await {
            violations.push(violation(
                QUANTITY_ROUND_TRIP,
                format!(
                    "`create_usage_record` refused the published quantity `{literal}` (record \
                     {id}): {err}"
                ),
            ));
            continue;
        }
        match read_back(plugin, id, window_end).await {
            Ok(observed) if observed == literal => {}
            Ok(observed) => violations.push(violation(
                QUANTITY_ROUND_TRIP,
                format!(
                    "submitted `{literal}` as record {id}; `list_usage_records` read back \
                     `{observed}`. The published range \
                     (`docs/usage-collector-v1.yaml:576-583`) obliges every storage plugin to \
                     round-trip it digit for digit, negative half included. The comparison is on \
                     the rendered decimal, so an equal magnitude under a normalised scale is \
                     still a loss."
                ),
            )),
            Err(detail) => violations.push(violation(QUANTITY_ROUND_TRIP, detail)),
        }
    }
    violations
}

/// Reads one fixture back and renders its quantity.
///
/// The range is `[window_end, window_end + 1s)`, which selects the entry
/// under `from <= window_end < to` and nothing else the check wrote — the
/// fixtures are an hour apart. `Err` carries a ready-to-report detail.
async fn read_back(
    plugin: &dyn UsageCollectorPluginV1,
    id: Uuid,
    window_end: time::OffsetDateTime,
) -> Result<String, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let upper = window_end.saturating_add(time::Duration::seconds(1));
    let range = TimeRange::new(window_end, upper)
        .map_err(|err| format!("the check could not build a range around record {id}: {err}"))?;
    let limit = u64::try_from(PUBLISHED_RANGE_CORNERS.len()).unwrap_or(u64::MAX);
    let page = plugin
        .list_usage_records(meter, range, &contract_query(limit), &[])
        .await
        .map_err(|err| {
            format!("`list_usage_records` failed while reading record {id} back: {err}")
        })?;
    page.items
        .into_iter()
        .find(|item| item.id == id)
        .map(|item| item.value.to_string())
        .ok_or_else(|| {
            format!(
                "record {id} was accepted by `create_usage_record` but a `list_usage_records` \
                 range containing its `window_end` did not return it, so its quantity could not \
                 be compared at all"
            )
        })
}

/// A published-range corner, paired with the record built to carry it.
type QuantityFixture = (&'static str, UsageRecord);

/// One fixture per corner of the published range, paired with the literal
/// it will be submitted as, alongside a violation per corner that could not
/// be built.
///
/// **Partitioned rather than short-circuited**, and attributed to
/// [`HARNESS_FAULT`] rather than to the check. Collecting into a `Result`
/// would let one unbuildable corner suppress the other four and report the
/// suite's own failure under a DESIGN check name — telling a plugin author
/// their plugin violated a contract it was never handed a value to violate.
/// The buildable corners are still submitted, so the run still says
/// whatever it can about the plugin.
pub fn quantity_fixtures() -> (Vec<QuantityFixture>, Vec<ContractViolation>) {
    let mut built = Vec::new();
    let mut faults = Vec::new();
    for (index, literal) in PUBLISHED_RANGE_CORNERS.iter().enumerate() {
        match quantity_fixture(index, literal) {
            Ok(fixture) => built.push(fixture),
            Err(detail) => faults.push(violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{literal}` fixture, so that \
                     corner was never submitted. This is a fault in the suite, not in the \
                     plugin under test: {detail}"
                ),
            )),
        }
    }
    (built, faults)
}

/// Builds the fixture for one corner, refusing to proceed if the carrier
/// itself cannot hold the published value.
///
/// The `to_string()` guard is not ceremony: the check compares renderings,
/// so a corner the carrier silently rounded on parse would make the
/// assertion vacuous — the read-back would match a value the plugin was
/// never asked to store. Failing here says the published bound and the
/// carrier have diverged, which is a finding about the contract rather
/// than about any plugin.
pub fn quantity_fixture(index: usize, literal: &'static str) -> Result<QuantityFixture, String> {
    let value = Decimal::from_str(literal).map_err(|err| {
        format!("`{literal}` is inside the published range but `rust_decimal::Decimal` cannot parse it: {err}")
    })?;
    let rendered = value.to_string();
    if rendered != literal {
        return Err(format!(
            "`{literal}` is inside the published range but `rust_decimal::Decimal` renders it as \
             `{rendered}`, so the carrier loses it before any plugin sees it"
        ));
    }

    // An hour apart, so a one-second read range around any corner's
    // `window_end` selects that corner alone.
    let hours = i64::try_from(index)
        .map_err(|err| format!("fixture index {index} does not fit an hour offset: {err}"))?;
    let window_end = FIXTURE_EPOCH.saturating_add(time::Duration::hours(hours));
    let window_start = window_end.saturating_sub(time::Duration::hours(1));

    // Keyed on the **literal**, never on its position. The entry `id`
    // derives from `(tenant, gts_type, idempotency_key, window_start,
    // window_end)` and the quantity is not an input, so an index-keyed
    // fixture would give a changed corner the id of the corner that used to
    // sit at that index — and the ledger is append-only with no delete
    // path, so every backend that ever ran the older suite would answer
    // `IdempotencyConflict` for ever, and this check would report it as the
    // plugin refusing a published quantity. Keying on the literal gives a
    // changed, reordered or inserted corner a fresh identity instead.
    // `IdempotencyKey` admits 256 bytes and the longest corner is 29
    // characters, so the literal fits whole and needs no hash.
    let idempotency_key = IdempotencyKey::new(format!("{QUANTITY_ROUND_TRIP}-{literal}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
    let record = fixture_record(&idempotency_key, value, window_start, window_end)
        .map_err(|detail| format!("for `{literal}`: {detail}"))?;
    Ok((literal, record))
}
