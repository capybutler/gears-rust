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
//! # Five of seven, and where the other two are
//!
//! DESIGN §3.3 tabulates seven checks. [`run_all`] currently runs the ones
//! in [`IMPLEMENTED_CHECKS`], and **an empty violation list is not a
//! statement about the rest**. The other two are named, not omitted:
//!
//! * [`BLOCKED_CHECKS`] — cannot be written against the SPI this gear
//!   declares, each with what unblocks it.
//! * [`UNWRITTEN_CHECKS`] — writable today, not yet written. Now empty:
//!   every check the current SPI can express is written.
//!
//! The three constants are asserted to partition DESIGN's seven exactly, so
//! the split is a fact the test suite keeps rather than a paragraph that
//! drifts: [`UNWRITTEN_CHECKS`] emptied itself as the work landed, and a
//! check that half-lands fails the partition. A caller reporting coverage
//! should report all three alongside the violations — which matters
//! because "run this suite" is the acceptance criterion for porting a
//! backend, and a suite that runs five checks must not read as a suite
//! that ran seven.
//!
//! # The reference backend
//!
//! [`reference::InMemoryReferencePlugin`] is the subject this suite is
//! validated against. It is a conforming backend, not a production one and
//! not a template — see its module docs, which also say why the noop plugin
//! cannot serve in its place.

use std::collections::BTreeSet;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use toolkit_gts::gts_id;
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use uuid::Uuid;

use crate::derive_usage_record_id;
use crate::error::UsageCollectorPluginError;
use crate::models::{
    AggregationFold, CreateUsageRecord, IdempotencyKey, Invalidation, MeterTypeId, RECORD_ID_FIELD,
    ReasonCode, RecordOrigin, ResourceRef, UsageRecord, WINDOW_END_FIELD,
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
    /// The DESIGN §3.3 check name, spelled as the table spells it — or
    /// [`HARNESS_FAULT`] when the suite itself failed rather than the
    /// plugin.
    pub check: &'static str,
    /// What was observed, and what the check required instead.
    pub detail: String,
}

impl std::fmt::Display for ContractViolation {
    /// One line per violation, `"<check>: <detail>"`.
    ///
    /// The `{violations:#?}` in this module's example is right for an
    /// `assert!` message and wrong for anything that reports per line, so
    /// the per-line rendering is the type's own rather than each caller's.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.check, self.detail)
    }
}

/// The DESIGN §3.3 `quantity-round-trip` check, spelled as the table spells
/// it. Exported so a caller can tell this suite's own check names apart
/// from the [`BLOCKED_CHECKS`] entries without matching a string literal.
pub const QUANTITY_ROUND_TRIP: &str = "quantity-round-trip";

/// The DESIGN §3.3 `window-end-selection` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const WINDOW_END_SELECTION: &str = "window-end-selection";

/// The DESIGN §3.3 `dedup-identity-over-window` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const DEDUP_IDENTITY_OVER_WINDOW: &str = "dedup-identity-over-window";

/// The DESIGN §3.3 `invalidation-excluded-from-fold` check, spelled as the
/// table spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const INVALIDATION_EXCLUDED_FROM_FOLD: &str = "invalidation-excluded-from-fold";

/// The DESIGN §3.3 `at-most-one-invalidation` check, spelled as the table
/// spells it. Exported for the same reason as [`QUANTITY_ROUND_TRIP`].
pub const AT_MOST_ONE_INVALIDATION: &str = "at-most-one-invalidation";

/// The [`ContractViolation::check`] value a violation carries when the
/// **suite itself** failed — it could not build a fixture, say — rather
/// than the plugin.
///
/// Deliberately not a DESIGN §3.3 check name, and deliberately not
/// [`QUANTITY_ROUND_TRIP`]: a report that attributes the harness's own
/// fault to a check tells a plugin author their plugin broke a contract it
/// never got to touch.
pub const HARNESS_FAULT: &str = "contract-suite-harness-fault";

/// The DESIGN §3.3 checks [`run_all`] actually runs.
///
/// Adding a check means adding it here as well as to [`run_all`] and
/// removing it from [`UNWRITTEN_CHECKS`]; the partition test refuses a
/// half-landed change.
pub const IMPLEMENTED_CHECKS: &[&str] = &[
    QUANTITY_ROUND_TRIP,
    WINDOW_END_SELECTION,
    DEDUP_IDENTITY_OVER_WINDOW,
    INVALIDATION_EXCLUDED_FROM_FOLD,
    AT_MOST_ONE_INVALIDATION,
];

/// The DESIGN §3.3 checks that are writable against the current SPI and are
/// not yet written.
///
/// **Empty.** Every check expressible with the five methods this gear
/// declares is written and run by [`run_all`]; what remains unimplemented
/// is in [`BLOCKED_CHECKS`], which needs the SPI to grow. The constant
/// stands rather than being deleted: it is one of the three the partition
/// test holds against DESIGN's seven, so a check that becomes writable and
/// is not yet written has a place to be named, and a check that half-lands
/// still fails the partition.
pub const UNWRITTEN_CHECKS: &[&str] = &[];

/// The two DESIGN §3.3 checks the current SPI cannot express, and why.
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
         section 1.2 has the plugin assign it strictly monotonic per \
         `(tenant_id, gts_type_id)`, restated as a storage obligation in \
         section 3.7; until the field exists there is nothing for a plugin \
         to assign or a fold to read.",
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
    let mut violations = quantity_round_trip(plugin).await;
    violations.extend(window_end_selection(plugin).await);
    violations.extend(dedup_identity_over_window(plugin).await);
    violations.extend(invalidation_excluded_from_fold(plugin).await);
    violations.extend(at_most_one_invalidation(plugin).await);
    violations
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

/// `2020-01-01T00:00:00Z`, the base every fixture covered period is offset
/// from.
///
/// Deliberately not [`time::OffsetDateTime::UNIX_EPOCH`]. A period ending
/// at the epoch starts an hour *before* it, and a backend whose time column
/// refuses a negative instant would then fail a check about decimals — the
/// suite would blame the plugin for the fixture's own choice of date.
const FIXTURE_EPOCH: time::OffsetDateTime =
    time::OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::days(18_262));

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
/// single-tenant grant whose constraint carries one filter: a bare
/// `Compare`, since the projection only builds a conjunction once there is
/// a second filter to AND.
///
/// What dispatching it buys is **shape coverage** — a plugin that chokes on
/// a filter, or ignores `query.filter` and therefore never exercises its
/// projection, meets one here. It buys no scope *enforcement*: every
/// fixture belongs to the tenant this filter pins, and no implemented check
/// asserts that a row outside the scope is withheld, so a backend that
/// discarded the filter entirely would still pass. That assertion belongs
/// to a scope-gating check, which is not written.
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

/// Builds one fixture entry over the shared meter, tenant and resource.
///
/// Only the three attributes a check actually varies are parameters: the
/// idempotency key and the two covered-period bounds are what the derived
/// identity reads, and the quantity is what `quantity-round-trip` probes.
/// Everything else is the shared vocabulary above, so two entries differ
/// exactly where the check meant them to.
///
/// The projection derives the entry's `id` and validates the period, so a
/// fixture with an inverted period or a bound finer than a microsecond is
/// refused here rather than reaching a plugin. `Err` carries a
/// ready-to-report detail.
fn fixture_record(
    idempotency_key: &IdempotencyKey,
    value: Decimal,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<UsageRecord, String> {
    CreateUsageRecord {
        gts_type_id: MeterTypeId::new(CONTRACT_METER_TYPE_ID)
            .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?,
        tenant_id: CONTRACT_TENANT_ID,
        resource_ref: ResourceRef::new(CONTRACT_RESOURCE_ID, CONTRACT_RESOURCE_TYPE)
            .map_err(|err| format!("the check's own resource reference is invalid: {err}"))?,
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        value,
        idempotency_key: idempotency_key.clone(),
        invalidation: None,
        window_start,
        window_end,
    }
    .try_into_usage_record(RecordOrigin::Live)
    .map_err(|err| format!("the check's own submission is not projectable: {err}"))
}

/// The reason every withdrawal this suite submits states.
///
/// The vocabulary is deliberately open — the gear records the emitter's
/// stated intent and infers nothing from it — so any well-formed code
/// serves and no check reads this one.
const CONTRACT_REASON_CODE: &str = "contract-suite-withdrawal";

/// Builds one invalidation entry: a fixture record carrying a withdrawal.
///
/// The withdrawal is attached after the projection rather than travelling
/// through it, and the entry's `id` is unaffected — [`Invalidation::target`]
/// is deliberately excluded from the derived identity
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`), so the id
/// [`fixture_record`] derived over the five identity attributes is the id
/// this entry has. What the exclusion costs is that the idempotency key
/// alone separates two withdrawals of one target, which is why every caller
/// here passes a distinct one.
fn fixture_invalidation(
    idempotency_key: &IdempotencyKey,
    value: Decimal,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
    target: Uuid,
) -> Result<UsageRecord, String> {
    let reason = ReasonCode::new(CONTRACT_REASON_CODE)
        .map_err(|err| format!("the check's own reason code is invalid: {err}"))?;
    Ok(UsageRecord {
        invalidation: Some(Invalidation { target, reason }),
        ..fixture_record(idempotency_key, value, window_start, window_end)?
    })
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
fn quantity_fixtures() -> (Vec<QuantityFixture>, Vec<ContractViolation>) {
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
fn quantity_fixture(index: usize, literal: &'static str) -> Result<QuantityFixture, String> {
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

// ---------------------------------------------------------------------------
// window-end-selection
// ---------------------------------------------------------------------------

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
const WINDOW_SELECTION_FROM: time::OffsetDateTime =
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
async fn window_end_selection(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
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
/// [`quantity_fixtures`] gives: one unbuildable row must not suppress the
/// other four, and the suite's own failure must not be reported under a
/// DESIGN check name.
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
/// id of whatever used to sit beside it — the hazard [`quantity_fixture`]
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

// ---------------------------------------------------------------------------
// dedup-identity-over-window
// ---------------------------------------------------------------------------

/// The start of the first of this check's two covered periods, and the
/// inclusive lower bound of the range it reads them back over.
///
/// Sixty days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`] gives.
const DEDUP_WINDOW_FROM: time::OffsetDateTime =
    FIXTURE_EPOCH.saturating_add(time::Duration::days(60));

/// The exclusive upper bound of that range, an hour past the end of the
/// later of the two periods so both are selected by their end.
const DEDUP_WINDOW_TO: time::OffsetDateTime =
    DEDUP_WINDOW_FROM.saturating_add(time::Duration::hours(3));

/// The read limit this check dispatches.
///
/// Twice the two entries it expects, and the margin is the assertion. A
/// backend that stored the same-period resubmission as a second row is
/// caught by counting the rows carrying one id, and a limit set to the
/// expected two would truncate that second row away — turning the half of
/// this check that catches a backend with no dedup at all into a pass.
const DEDUP_PAGE_LIMIT: u64 = 4;

/// The two submissions this check works with: one idempotency key, two
/// covered periods.
struct DedupFixtures {
    /// The entry over the earlier period.
    first: UsageRecord,
    /// The entry over the later one, submitted under the same key.
    second: UsageRecord,
    /// [`derive_usage_record_id`] over the earlier period's five identity
    /// attributes — derived here rather than read off [`Self::first`], so
    /// the check names the entry the way the gear does.
    first_id: Uuid,
    /// The same derivation over the later period's.
    second_id: Uuid,
}

/// `dedup-identity-over-window` — *"Both period bounds are part of the
/// identity, so a same-key submission over a different period is a distinct
/// entry."*
///
/// **Both halves are asserted, and neither is sufficient alone.**
///
/// The first half is the rule as DESIGN states it: one idempotency key over
/// two different periods is two entries, both admitted and both readable,
/// under the two distinct ids [`derive_usage_record_id`] produces from the
/// five identity attributes. That half on its own is satisfied by a backend
/// that dedups nothing whatsoever — two submissions produced two rows is
/// exactly what no deduplication looks like.
///
/// The second half is what closes that: the same key over the **same**
/// period, every canonical field identical, is an idempotent replay. The
/// stored row comes back and `list_usage_records` shows one entry, not two.
///
/// What the pair catches is a backend keying dedup on
/// `(tenant_id, gts_type_id, idempotency_key)` alone — the obvious schema,
/// and the one the pre-period model had. It answers `IdempotencyConflict`
/// to the second period's submission and so fails the first half, while
/// passing the second and every other check in this suite: the bounds are
/// invisible to it, and nothing else here submits one key over two periods.
async fn dedup_identity_over_window(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let fixtures = match dedup_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own \
                     `{DEDUP_IDENTITY_OVER_WINDOW}` fixtures, so nothing was submitted. This is \
                     a fault in the suite, not in the plugin under test: {detail}"
                ),
            )];
        }
    };
    let mut violations = Vec::new();

    // Half one: one key, two periods, two entries.
    let mut submitted = Vec::new();
    for (ordinal, record, expected) in [
        ("earlier", &fixtures.first, fixtures.first_id),
        ("later", &fixtures.second, fixtures.second_id),
    ] {
        if let Err(err) = plugin.create_usage_record(record.clone()).await {
            violations.push(violation(
                DEDUP_IDENTITY_OVER_WINDOW,
                format!(
                    "`create_usage_record` refused the {ordinal} of two submissions carrying the \
                     idempotency key `{key}` over different covered periods (record {expected}, \
                     period `{start}` to `{end}`): {err}. Both period bounds are part of the \
                     dedup identity, so the two are distinct entries and both must be admitted; \
                     a backend keying dedup on `(tenant_id, gts_type_id, idempotency_key)` alone \
                     reports a conflict here.",
                    key = record.idempotency_key.as_str(),
                    start = record.window_start,
                    end = record.window_end,
                ),
            ));
            continue;
        }
        submitted.push((ordinal, expected));
    }

    let returned = match dedup_page(plugin).await {
        Ok(returned) => returned,
        Err(detail) => {
            violations.push(violation(DEDUP_IDENTITY_OVER_WINDOW, detail));
            return violations;
        }
    };
    for (ordinal, expected) in submitted {
        if returned.contains(&expected) {
            continue;
        }
        violations.push(violation(
            DEDUP_IDENTITY_OVER_WINDOW,
            format!(
                "the {ordinal} of two submissions carrying one idempotency key over different \
                 covered periods was accepted as record {expected}, and a range containing both \
                 periods' ends did not return it. A same-key submission over a different period \
                 is a distinct entry, so both must be readable under the two distinct ids \
                 `derive_usage_record_id` produces."
            ),
        ));
    }

    // Half two: the same key over the *same* period is one entry, not two.
    match plugin.create_usage_record(fixtures.first.clone()).await {
        Ok(stored) if stored == fixtures.first => {}
        Ok(stored) => violations.push(violation(
            DEDUP_IDENTITY_OVER_WINDOW,
            format!(
                "resubmitting record {expected} verbatim answered a different entry ({observed}). \
                 A re-delivery of an accepted entry is an idempotent replay: the stored row comes \
                 back unchanged.",
                expected = fixtures.first_id,
                observed = stored.id,
            ),
        )),
        Err(err) => violations.push(violation(
            DEDUP_IDENTITY_OVER_WINDOW,
            format!(
                "resubmitting record {expected} verbatim, under the same idempotency key over \
                 the same covered period with every canonical field identical, was refused: \
                 {err}. A re-delivery of an accepted entry is an idempotent replay, not a \
                 conflict.",
                expected = fixtures.first_id,
            ),
        )),
    }

    match dedup_page(plugin).await {
        Ok(returned) => {
            let seen = returned
                .iter()
                .filter(|id| **id == fixtures.first_id)
                .count();
            if seen != 1 {
                violations.push(violation(
                    DEDUP_IDENTITY_OVER_WINDOW,
                    format!(
                        "record {expected} was submitted twice over one covered period under one \
                         idempotency key, and `list_usage_records` returned it {seen} times \
                         rather than once. Without this the check passes against a backend that \
                         deduplicates nothing at all, for which two submissions producing two \
                         rows is the correct-looking answer.",
                        expected = fixtures.first_id,
                    ),
                ));
            }
        }
        Err(detail) => violations.push(violation(DEDUP_IDENTITY_OVER_WINDOW, detail)),
    }

    violations
}

/// Builds the two submissions, deriving each entry's id the way the gear
/// does.
///
/// The ids come from [`derive_usage_record_id`] over the five identity
/// attributes rather than being hardcoded or read back off the projected
/// record, and the two guards below are what keep the check honest rather
/// than vacuous:
///
/// * The projection derives the same id it does. They cannot disagree —
///   the projection calls the same function — and if they ever did, every
///   read-back assertion would look for an id nothing was stored under and
///   blame the plugin for the suite's own confusion.
/// * The two periods derive **different** ids. If they did not, "a same-key
///   submission over a different period is a distinct entry" would have
///   nothing to assert and the check would pass by construction.
///
/// Both are the suite's own facts, so both are `Err` here and reported as
/// [`HARNESS_FAULT`] rather than against the plugin.
fn dedup_fixtures() -> Result<DedupFixtures, String> {
    let idempotency_key = IdempotencyKey::new(DEDUP_IDENTITY_OVER_WINDOW)
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;

    let middle = DEDUP_WINDOW_FROM.saturating_add(time::Duration::hours(1));
    let last = DEDUP_WINDOW_FROM.saturating_add(time::Duration::hours(2));
    let periods = [(DEDUP_WINDOW_FROM, middle), (middle, last)];

    let mut records = Vec::with_capacity(periods.len());
    for (window_start, window_end) in periods {
        let derived = derive_usage_record_id(
            CONTRACT_TENANT_ID,
            &meter,
            &idempotency_key,
            window_start,
            window_end,
        );
        let record = fixture_record(&idempotency_key, Decimal::ONE, window_start, window_end)?;
        if record.id != derived {
            return Err(format!(
                "the suite derived {derived} for the period `{window_start}` to `{window_end}` \
                 and projected the same submission as {projected}, so its two ways of naming one \
                 entry disagree",
                projected = record.id,
            ));
        }
        records.push((record, derived));
    }

    let mut records = records.into_iter();
    let (first, first_id) = records
        .next()
        .ok_or("the check built no fixture for its earlier period")?;
    let (second, second_id) = records
        .next()
        .ok_or("the check built no fixture for its later period")?;
    if first_id == second_id {
        return Err(format!(
            "one idempotency key over two different covered periods derived the same id \
             ({first_id}), so there is nothing for this check to assert"
        ));
    }
    Ok(DedupFixtures {
        first,
        second,
        first_id,
        second_id,
    })
}

/// Every `UsageRecord.id` a range containing both periods' ends comes back
/// with, in the order the page carried them.
///
/// A `Vec` rather than a set, because half two counts repetitions: a set
/// would collapse the duplicate row it exists to find.
async fn dedup_page(plugin: &dyn UsageCollectorPluginV1) -> Result<Vec<Uuid>, String> {
    let meter = MeterTypeId::new(CONTRACT_METER_TYPE_ID)
        .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?;
    let range = TimeRange::new(DEDUP_WINDOW_FROM, DEDUP_WINDOW_TO)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let page = plugin
        .list_usage_records(meter, range, &contract_query(DEDUP_PAGE_LIMIT), &[])
        .await
        .map_err(|err| {
            format!(
                "`list_usage_records` failed over the range containing both covered periods, so \
                 neither half of the dedup identity could be decided: {err}"
            )
        })?;
    Ok(page.items.into_iter().map(|item| item.id).collect())
}

// ---------------------------------------------------------------------------
// invalidation-excluded-from-fold
// ---------------------------------------------------------------------------

/// The start of the live entry's covered period, and the inclusive lower
/// bound of the range this check folds and reads over.
///
/// Ninety days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`] gives. It matters here for the same reason it
/// matters there: this check counts the rows a range returns, so a stray
/// entry from another check inside it would be read as a fourth entry.
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
/// three answers a backend can give are three different numbers: `7.25`
/// when the withdrawn pair is excluded, `1007.25` when only the record is,
/// and `2007.25` when neither is.
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
/// The margin is the assertion, for the reason [`DEDUP_PAGE_LIMIT`] gives.
/// A limit set to the expected three would truncate a fourth row away, and
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
async fn invalidation_excluded_from_fold(
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
/// Keyed on the role name for the reason [`quantity_fixture`] spells out:
/// in a ledger with no delete path, an edited fixture must take a fresh
/// identity rather than inherit an accepted entry's.
fn fold_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{INVALIDATION_EXCLUDED_FROM_FOLD}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}

// ---------------------------------------------------------------------------
// at-most-one-invalidation
// ---------------------------------------------------------------------------

/// The start of the sequential half's covered period, and the instant this
/// check's four entries are arranged from.
///
/// A hundred and twenty days past [`FIXTURE_EPOCH`], for the reason
/// [`WINDOW_SELECTION_FROM`] gives. This check reads nothing back, so the
/// separation buys less here than elsewhere — it keeps these entries out of
/// the ranges the checks that *do* read back dispatch.
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
async fn at_most_one_invalidation(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
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
/// keyed on the role name for the reason [`quantity_fixture`] gives.
fn at_most_one_key(role: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(format!("{AT_MOST_ONE_INVALIDATION}-{role}"))
        .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))
}

/// A [`ContractViolation`] attributed to one check.
///
/// The check name is a parameter rather than baked in. Five checks report
/// through it and [`HARNESS_FAULT`] is a sixth caller, and the whole point
/// of [`ContractViolation::check`] is that a violation says which assertion
/// produced it — a helper that stamped one name on every report would
/// quietly undo that.
fn violation(check: &'static str, detail: String) -> ContractViolation {
    ContractViolation { check, detail }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "contract_tests.rs"]
mod contract_tests;
