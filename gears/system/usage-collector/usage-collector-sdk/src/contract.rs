//! The DESIGN §3.3 plugin contract suite.
//!
//! DESIGN §3.3 "Plugin SPI" says: *"Every conforming plugin MUST pass the
//! suite in `usage-collector-sdk`. The tests are behavioural and MUST pass
//! on any backend."* That obligation is on the plugin, so the suite cannot
//! live in this crate's `tests/` directory — an integration-test target is
//! private to its own crate and no plugin crate can reach into it. It is a
//! public, feature-gated module instead, and this crate's own tests are one
//! caller of it rather than its home.
//!
//! The checks are behavioural: they submit entries through
//! [`UsageCollectorPluginV1`] and read them back through the same trait.
//! Nothing here inspects a backend's storage, so a SQL-backed plugin, an
//! in-memory one and a remote one are all subject to the same assertions.
//!
//! # Running it against your plugin
//!
//! Enable the `contract` feature on the dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! cf-gears-usage-collector-sdk = { workspace = true, features = ["contract"] }
//! ```
//!
//! then call [`run_all`] from an ordinary async test:
//!
//! ```rust,ignore
//! use usage_collector_sdk::contract;
//!
//! #[tokio::test]
//! async fn conforms_to_the_plugin_contract() {
//!     let plugin = MyBackend::connect(&test_database_url()).await;
//!     let violations = contract::run_all(&plugin).await;
//!     assert!(violations.is_empty(), "plugin contract violations: {violations:#?}");
//! }
//! ```
//!
//! Every entry of the returned vector names the check that produced it
//! ([`ContractViolation::check`]), so one run reports every failure rather
//! than stopping at the first. The suite writes entries and never removes
//! them, so a backend under test starts each run from whatever state the
//! previous one left; the fixtures are keyed so that a repeated run
//! resubmits identical entries rather than colliding with different ones.
//!
//! # Five of seven
//!
//! DESIGN §3.3 tabulates seven checks. Two of them cannot be written
//! against the SPI this gear declares, and they are named in
//! [`BLOCKED_CHECKS`] — with what unblocks each — rather than silently
//! omitted, so a passing run is never mistaken for a complete one. A caller
//! reporting coverage should report that constant alongside the violations.
//!
//! # The reference backend
//!
//! [`reference::InMemoryReferencePlugin`] is the subject this suite is
//! validated against. It is a conforming backend, not a production one and
//! not a template — see its module docs, which also say why the noop plugin
//! cannot serve in its place.

use std::str::FromStr;

use rust_decimal::Decimal;
use toolkit_gts::gts_id;
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use uuid::Uuid;

use crate::models::{
    CreateUsageRecord, IdempotencyKey, MeterTypeId, RECORD_ID_FIELD, RecordOrigin, ResourceRef,
    UsageRecord, WINDOW_END_FIELD,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

pub mod reference;

/// A check that failed, naming the check and what was observed.
///
/// Carries the check's own name so a caller can report a whole run without
/// re-deriving which assertion produced which failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractViolation {
    /// The DESIGN §3.3 check name, spelled as the table spells it.
    pub check: &'static str,
    /// What was observed, and what the check required instead.
    pub detail: String,
}

/// The DESIGN §3.3 `quantity-round-trip` check, spelled as the table spells
/// it. Exported so a caller can tell this suite's own check names apart
/// from the [`BLOCKED_CHECKS`] entries without matching a string literal.
pub const QUANTITY_ROUND_TRIP: &str = "quantity-round-trip";

/// The two DESIGN §3.3 checks this suite does not implement, and why.
pub const BLOCKED_CHECKS: &[(&str, &str)] = &[
    (
        "feed-snapshot-and-replay",
        "the gear's SPI declares no feed method: DESIGN section 3.3 gives \
         `UsageCollectorPluginV1` a `read_feed_page`, and this gear \
         implements five methods, none of which reads a feed. Unblocked by \
         the usage feed.",
    ),
    (
        "latest-tie-break",
        "asserts `greatest window_end, then greatest acceptance_sequence`, \
         and `UsageRecord` carries no `acceptance_sequence` field. DESIGN \
         section 3.1 has the plugin assign it monotonically per \
         `(tenant_id, gts_type_id)`; until the field exists there is nothing \
         for a plugin to assign or a fold to read.",
    ),
];

/// Run every implemented check, returning one entry per violation.
///
/// An empty result means the plugin conforms as far as this suite reaches.
/// The checks are run in sequence rather than concurrently: several store
/// entries and then read them back, and interleaving them would let one
/// check observe another's rows.
///
/// The plugin arrives behind `&dyn` rather than a generic parameter. The
/// suite is dispatched once per backend and its cost is entirely in the
/// awaits, so monomorphising it buys nothing; erasing it means the host's
/// own handle — `ClientHub` hands out a `dyn UsageCollectorPluginV1` —
/// passes straight in.
pub async fn run_all(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    quantity_round_trip(plugin).await
}

// ---------------------------------------------------------------------------
// Shared fixture vocabulary
// ---------------------------------------------------------------------------

/// The meter every fixture entry attaches to. A single derived type is
/// enough: no implemented check reads a declaration, and the SPI never sees
/// one — the fold arrives as a parameter.
const CONTRACT_METER_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.core.uc.contract_suite.v1~");

/// The tenant every fixture entry is attributed to, and the one value the
/// scope filter the suite dispatches pins.
const CONTRACT_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0001);

/// The resource every fixture entry is attributed to. `resource_ref` is
/// mandatory on a [`UsageRecord`] and no implemented check varies it.
const CONTRACT_RESOURCE_ID: &str = "contract-suite-resource";
/// The resource-type discriminator paired with [`CONTRACT_RESOURCE_ID`].
const CONTRACT_RESOURCE_TYPE: &str = "contract.suite";

/// The `filter_hash` the suite dispatches with.
///
/// The SPI guarantees the slot is populated on every `list_usage_records`
/// dispatch and obliges a plugin to carry it through verbatim into any
/// cursor it mints, so a suite that left it `None` would exercise a shape
/// the gateway never produces.
const CONTRACT_FILTER_HASH: &str = "usage-collector-contract-suite";

/// The compiled PDP scope the suite dispatches: `tenant_id eq <tenant>`.
///
/// This is the shape `authz::scope_to_odata_filter` projects for a
/// single-tenant grant — a conjunction pinning the owning tenant. Passing
/// no filter at all would let a backend that ignores the scope pass the
/// suite.
fn contract_scope() -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(CONTRACT_TENANT_ID))),
    )
}

/// The query the suite's read paths dispatch: the compiled scope as the
/// filter, the canonical `(window_end, id)` keyset as the order, and the
/// `filter_hash` the gateway guarantees.
fn contract_query(limit: u64) -> ODataQuery {
    ODataQuery::new()
        .with_filter(contract_scope())
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: WINDOW_END_FIELD.to_owned(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: RECORD_ID_FIELD.to_owned(),
                dir: SortDir::Asc,
            },
        ]))
        .with_limit(limit)
        .with_filter_hash(CONTRACT_FILTER_HASH.to_owned())
}

// ---------------------------------------------------------------------------
// quantity-round-trip
// ---------------------------------------------------------------------------

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
async fn quantity_round_trip(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let fixtures = match quantity_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(format!(
                "the check could not build its fixtures, so nothing was asserted: {detail}"
            ))];
        }
    };

    let mut violations = Vec::new();
    for (literal, record) in fixtures {
        let id = record.id;
        let window_end = record.window_end;
        if let Err(err) = plugin.create_usage_record(record).await {
            violations.push(violation(format!(
                "`create_usage_record` refused the published quantity `{literal}` (record {id}): {err}"
            )));
            continue;
        }
        match read_back(plugin, id, window_end).await {
            Ok(observed) if observed == literal => {}
            Ok(observed) => violations.push(violation(format!(
                "submitted `{literal}` as record {id}; `list_usage_records` read back `{observed}`. \
                 The published range (`docs/usage-collector-v1.yaml:576-583`) obliges every storage \
                 plugin to round-trip it digit for digit, negative half included — the comparison is \
                 on the rendered decimal, so an equal magnitude under a normalised scale is still a \
                 loss."
            ))),
            Err(detail) => violations.push(violation(detail)),
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

/// One persisted fixture per corner of the published range, paired with the
/// literal it was submitted as.
fn quantity_fixtures() -> Result<Vec<(&'static str, UsageRecord)>, String> {
    PUBLISHED_RANGE_CORNERS
        .iter()
        .enumerate()
        .map(|(index, literal)| quantity_fixture(index, literal))
        .collect()
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
fn quantity_fixture(
    index: usize,
    literal: &'static str,
) -> Result<(&'static str, UsageRecord), String> {
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
    let window_end = time::OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::hours(hours));
    let window_start = window_end.saturating_sub(time::Duration::hours(1));

    let submission = CreateUsageRecord {
        gts_type_id: MeterTypeId::new(CONTRACT_METER_TYPE_ID)
            .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?,
        tenant_id: CONTRACT_TENANT_ID,
        resource_ref: ResourceRef::new(CONTRACT_RESOURCE_ID, CONTRACT_RESOURCE_TYPE)
            .map_err(|err| format!("the check's own resource reference is invalid: {err}"))?,
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        value,
        idempotency_key: IdempotencyKey::new(format!("{QUANTITY_ROUND_TRIP}-{index}"))
            .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?,
        invalidation: None,
        window_start,
        window_end,
    };
    let record = submission
        .try_into_usage_record(RecordOrigin::Live)
        .map_err(|err| {
            format!("the check's own submission for `{literal}` is not projectable: {err}")
        })?;
    Ok((literal, record))
}

/// A [`ContractViolation`] attributed to [`QUANTITY_ROUND_TRIP`].
fn violation(detail: String) -> ContractViolation {
    ContractViolation {
        check: QUANTITY_ROUND_TRIP,
        detail,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "contract_tests.rs"]
mod contract_tests;
