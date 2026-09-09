//! The vocabulary every check builds its fixtures from.
//!
//! One meter, one tenant and one resource are shared across the whole
//! suite, deliberately: with everything else held fixed, nothing but an
//! entry's covered period can decide it, and each check separates its own
//! entries from every other check's by offsetting them from
//! [`FIXTURE_EPOCH`]. The builders here are the only place a
//! [`crate::models::UsageRecord`] is projected, so two entries differ
//! exactly where the check that asked for them meant them to.
//!
//! [`violation`] lives here for the same reason: every check in
//! [`super::checks`] reports through it, and so does
//! [`super::HARNESS_FAULT`].

use rust_decimal::Decimal;
use toolkit_gts::gts_id;
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use uuid::Uuid;

use super::ContractViolation;
use crate::models::{
    CreateUsageRecord, IdempotencyKey, Invalidation, MeterTypeId, RECORD_ID_FIELD, ReasonCode,
    RecordOrigin, ResourceRef, UsageRecord, WINDOW_END_FIELD,
};

/// The meter every fixture entry attaches to. A single derived type is
/// enough: no implemented check reads a declaration, and the SPI never sees
/// one — the fold arrives as a parameter.
pub const CONTRACT_METER_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.core.uc.contract_suite.v1~");

/// The tenant every fixture entry is attributed to, and the one value the
/// scope filter the suite dispatches pins.
pub const CONTRACT_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0001);

/// `2020-01-01T00:00:00Z`, the base every fixture covered period is offset
/// from.
///
/// Deliberately not [`time::OffsetDateTime::UNIX_EPOCH`]. A period ending
/// at the epoch starts an hour *before* it, and a backend whose time column
/// refuses a negative instant would then fail a check about decimals — the
/// suite would blame the plugin for the fixture's own choice of date.
pub const FIXTURE_EPOCH: time::OffsetDateTime =
    time::OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::days(18_262));

/// The resource every fixture entry is attributed to. `resource_ref` is
/// mandatory on a [`UsageRecord`] and no implemented check varies it.
pub const CONTRACT_RESOURCE_ID: &str = "contract-suite-resource";
/// The resource-type discriminator paired with [`CONTRACT_RESOURCE_ID`].
pub const CONTRACT_RESOURCE_TYPE: &str = "contract.suite";

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
pub fn contract_query(limit: u64) -> ODataQuery {
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
pub fn fixture_record(
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
pub fn fixture_invalidation(
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

/// A [`ContractViolation`] attributed to one check.
///
/// The check name is a parameter rather than baked in. Five checks report
/// through it and [`HARNESS_FAULT`](super::HARNESS_FAULT) is a sixth
/// caller, and the whole point of [`ContractViolation::check`] is that a
/// violation says which assertion produced it — a helper that stamped one
/// name on every report would quietly undo that.
pub fn violation(check: &'static str, detail: String) -> ContractViolation {
    ContractViolation { check, detail }
}
