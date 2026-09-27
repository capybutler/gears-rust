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
//! A check the sharing does not suit derives its own vocabulary from the
//! same place: [`check_meter`] mints a meter for one check's exclusive use,
//! and [`contract_tenant`] a tenant from a block no named id falls in. A
//! feed read is what needs them: its subscription selects by meter, so
//! whatever another check left on [`CONTRACT_METER_TYPE_ID`] under the
//! grant it dispatches lands on the same page, and what the check observed
//! would turn on the order `super::run_all` happened to dispatch in.
//!
//! [`violation`] lives here for the same reason: every check in
//! [`super::checks`] reports through it, and so does
//! [`super::HARNESS_FAULT`].

use toolkit_gts::gts_id;
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use uuid::Uuid;

use super::ContractViolation;
use crate::models::{
    CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, RECORD_ID_FIELD, ReasonCode,
    RecordOrigin, ResourceRef, USAGE_RECORD_BASE_TYPE, UsageRecord, WINDOW_END_FIELD,
};
use crate::quantity::UsageQuantity;

/// The meter the shared builders below attach an entry to. A single derived
/// type is enough for them: no implemented check reads a declaration, and
/// the SPI never sees one — the fold arrives as a parameter.
///
/// A check that cannot share it derives one of its own through
/// [`check_meter`].
pub const CONTRACT_METER_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.core.uc.contract_suite.v1~");

/// Derives a meter for one check's exclusive use.
///
/// [`CONTRACT_METER_TYPE_ID`] is shared by the whole suite, and a check that
/// selects entries by meter — a feed read, whose subscription is a list of
/// meters — cannot live with that. `super::run_all` dispatches every check
/// against one persistent backend that writes entries and never removes
/// them, so a page read over the shared meter carries whatever the other
/// checks left on it, and what the check observes turns on the order the
/// suite happened to run in. A meter derived here is written and read by
/// one check alone.
///
/// `role` separates the meters one check needs from each other — a check
/// that subscribes to one meter and deliberately writes to a second, say.
///
/// **Collision-freedom is structural rather than asserted.** Check names are
/// unique, and `the_three_coverage_constants_partition_the_design_checks` is
/// what holds them so: a name appearing in two coverage constants fails that
/// partition. Two derivations therefore collide only when one check passes
/// one `role` twice, which is that check's own doing and visible in it.
///
/// Every `-` in the pair becomes `_`. The `gts-id` grammar admits only
/// `[a-z0-9_]` inside a token and every DESIGN §3.3 check name is
/// hyphenated; a segment needs five dot-separated tokens
/// (`vendor.package.namespace.type.vMAJOR`), which `cf.core.uc.<slug>.v1`
/// satisfies.
///
/// `Err` carries a ready-to-report detail. That every check name derives at
/// all is asserted by `every_check_name_derives_a_distinct_valid_meter`
/// rather than left to the check that calls this, because the failure would
/// otherwise reach a plugin author as a harness fault against a conforming
/// backend.
///
/// **The dead-code exemption retired itself, exactly as it was built to.**
/// Nothing in the non-test build used to call this, so the exemption was an
/// `expect` rather than an `allow`, gated to `not(test)` because under
/// `cfg(test)` the function *was* called and a bare `expect` would have gone
/// unfulfilled there.
/// [`record_and_invalidation_distinct_identity`](super::checks::record_and_invalidation_distinct_identity())
/// is the check that landed and called it, and that call is ordinary
/// feature-gated library code — `mod checks` is not `cfg(test)` — so
/// `dead_code` stopped firing and `unfulfilled_lint_expectations` fired in
/// its place. That is a warning by default and an error under the
/// `-D warnings` this workspace's clippy target passes, so the attribute had
/// to go, and it went without anyone having to remember it. The same shape
/// still guards [`contract_tenant`] below, which has no caller yet.
pub fn check_meter(check: &str, role: &str) -> Result<MeterTypeId, String> {
    let slug = format!("{check}_{role}").replace('-', "_");
    MeterTypeId::new(format!("{USAGE_RECORD_BASE_TYPE}cf.core.uc.{slug}.v1~"))
        .map_err(|err| format!("the check's own derived meter id `{slug}` is invalid: {err}"))
}

/// The tenant every fixture entry is attributed to, and the one value the
/// scope filter the suite dispatches pins.
///
/// First of the suite's tenant ids. They are all minted in this module, and
/// the assertion under [`FEED_OTHER_TENANT_ID`] says why.
pub const CONTRACT_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0001);

/// The tenant the dispatched scope does **not** admit, read by
/// [`scope_is_a_filter_on_every_read_path`](super::checks::scope_is_a_filter_on_every_read_path()).
/// That check's own module says what it is for.
pub const SCOPE_EXCLUDED_TENANT_ID: Uuid =
    Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0002);

/// The tenant the dispatched scope's second disjunct names and no entry
/// carries, read by the same check. Its module says what it is for, and what
/// it buys depends on its owning no entry.
pub const SCOPE_UNUSED_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0003);

/// The tenant the reference backend's feed reads withhold from one grant and
/// hand to another. `super::contract_tests` says what it is for.
pub const FEED_OTHER_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0004);

/// The floor of the block [`contract_tenant`] mints from, and the id it
/// mints at index zero.
///
/// Above all four named ids, established when the crate compiles by the
/// second assertion in the block below.
const CONTRACT_TENANT_BLOCK: u128 = 0xc047_c047_0000_4000_8000_0001_0000_0000;

// Every tenant id above is distinct, and every one of them is below
// `CONTRACT_TENANT_BLOCK`. Both are established when the crate compiles.
//
// The ids are minted here rather than in each module that reads them, and
// this is the whole reason. `super::run_all` dispatches every check against
// one shared, persistent backend that writes entries and never removes them,
// so one check's fixtures are visible to the next; `SCOPE_UNUSED_TENANT_ID`'s
// entire assertion value is that it owns no entry. Two checks minting one id
// therefore dissolves that premise silently rather than failing anything —
// and it has happened: `FEED_OTHER_TENANT_ID` was a second `...0003` literal
// in `contract_tests.rs`, under a doc comment claiming it was distinct from
// `SCOPE_UNUSED_TENANT_ID`. A doc comment cannot hold that; this can.
//
// `contract_tenant` reaches that same premise, and by the same route: it is
// a factory rather than a literal, so nothing about its call sites says
// which ids it hands out. The second assertion is what keeps its block clear
// of the four named ids. Were it not, the factory could hand a check
// `SCOPE_UNUSED_TENANT_ID`, and whether the scope check's claim that that id
// owns no entry still held would depend on which of the two `run_all`
// dispatched first.
const _: () = {
    let ids = [
        CONTRACT_TENANT_ID.as_u128(),
        SCOPE_EXCLUDED_TENANT_ID.as_u128(),
        SCOPE_UNUSED_TENANT_ID.as_u128(),
        FEED_OTHER_TENANT_ID.as_u128(),
    ];
    let mut first = 0;
    while first < ids.len() {
        let mut second = first + 1;
        while second < ids.len() {
            assert!(
                ids[first] != ids[second],
                "two of the contract suite's tenant ids are one value: the suite runs every \
                 check against one shared backend that never removes an entry, so an id a \
                 second check mints is an id already carrying entries that check did not write"
            );
            second += 1;
        }
        first += 1;
    }
    let mut named = 0;
    while named < ids.len() {
        assert!(
            ids[named] < CONTRACT_TENANT_BLOCK,
            "a named tenant id falls inside the block `contract_tenant` mints from: the factory \
             would then hand some check an id another check already owns entries under, and \
             `SCOPE_UNUSED_TENANT_ID`'s assertion that it owns none would turn on which check \
             `run_all` dispatched first"
        );
        named += 1;
    }
};

/// Mints the `index`-th tenant of a block disjoint from every named id.
///
/// [`FEED_POSITION_BOUNDED`](super::FEED_POSITION_BOUNDED)'s rule is about a
/// subscription spanning **many** tenants — DESIGN §3.3 gives it as *"A
/// position issued for a subscription spanning many tenants encodes to the
/// same size as one spanning few, so the wire cursor holding it stays inside
/// its bound"* — so a check asserting it needs more tenants than the four
/// named above, and needs them without colliding with those four.
///
/// That the block is clear of those four is a compile-time assertion rather
/// than a matter of inspection; the comment on that assertion says why an
/// overlap would be worse than a failing check.
///
/// The dead-code exemption is the self-retiring form [`check_meter`]'s docs
/// explain, for the same reason.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "no caller until a check in `super::checks` needs a tenant beyond the four \
                  named above; the first one that does leaves this expectation unfulfilled"
    )
)]
#[must_use]
pub fn contract_tenant(index: u32) -> Uuid {
    Uuid::from_u128(CONTRACT_TENANT_BLOCK + u128::from(index))
}

/// `2020-01-01T00:00:00Z`, the base every fixture covered period is offset
/// from.
///
/// Deliberately not [`time::OffsetDateTime::UNIX_EPOCH`]. A period ending
/// at the epoch starts an hour *before* it, and a backend whose time column
/// refuses a negative instant would then fail a check about decimals — the
/// suite would blame the plugin for the fixture's own choice of date.
pub const FIXTURE_EPOCH: time::OffsetDateTime =
    time::OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::days(18_262));

/// The acceptance instant every fixture entry carries. Fixed rather than read
/// from the clock so a check's expected entries compare equal to what it
/// wrote; no slice-A check varies it.
pub const CONTRACT_ACCEPTED_AT: time::OffsetDateTime =
    time::OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::days(20_454));

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
/// fixture the other checks build belongs to the tenant this filter pins,
/// so a backend that discarded the filter entirely would still pass them.
/// Enforcement is
/// [`scope_is_a_filter_on_every_read_path`](super::checks::scope_is_a_filter_on_every_read_path())'s job, and that
/// check dispatches a scope of its own through
/// [`contract_query_with_scope`] rather than this one, because a scope
/// every fixture satisfies cannot discriminate.
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
    contract_query_with_scope(limit, contract_scope())
}

/// The same query under a caller-supplied compiled scope.
///
/// The gateway ANDs the compiled scope into `query.filter` before it
/// dispatches, so a plugin reads a scope on the two collection paths
/// through that slot and nowhere else. A check asserting the scope is
/// *enforced* rather than merely accepted has to put its own expression
/// there — one that admits some stored rows and withholds others — which
/// [`contract_query`]'s single-tenant filter cannot do, since every fixture
/// it meets belongs to that tenant.
pub fn contract_query_with_scope(limit: u64, scope: ast::Expr) -> ODataQuery {
    ODataQuery::new()
        .with_filter(scope)
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
    quantity: UsageQuantity,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<UsageRecord, String> {
    fixture_record_for_tenant(
        CONTRACT_TENANT_ID,
        idempotency_key,
        quantity,
        window_start,
        window_end,
    )
}

/// The same entry attributed to a caller-chosen tenant.
///
/// Two callers need it, and both for the same reason: a read that gates on
/// the scope has nothing to assert unless two stored entries fall on
/// opposite sides of the scope it dispatches.
/// [`scope_is_a_filter_on_every_read_path`](super::checks::scope_is_a_filter_on_every_read_path())
/// is the check; `super::contract_tests`'s `feed_ledger` is the other, and it
/// alternates the two tenants so that every page of a feed read carries some
/// entries and withholds others.
///
/// `tenant_id` is one of the six inputs the derived identity reads, so two
/// entries differing only here are two entries rather than an idempotent
/// replay of one.
///
/// The meter is [`CONTRACT_METER_TYPE_ID`] and the acceptance instant
/// [`CONTRACT_ACCEPTED_AT`]. A check that has to vary either goes to
/// [`fixture_record_on`], which this delegates to.
pub fn fixture_record_for_tenant(
    tenant_id: Uuid,
    idempotency_key: &IdempotencyKey,
    quantity: UsageQuantity,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<UsageRecord, String> {
    fixture_record_on(
        MeterTypeId::new(CONTRACT_METER_TYPE_ID)
            .map_err(|err| format!("the check's own meter type id is invalid: {err}"))?,
        tenant_id,
        idempotency_key,
        quantity,
        CONTRACT_ACCEPTED_AT,
        window_start,
        window_end,
    )
}

/// Builds one fixture entry on a caller-chosen meter, tenant and acceptance
/// instant — the general builder the two above delegate to.
///
/// **`accepted_at` is a parameter here and [`CONTRACT_ACCEPTED_AT`] in both
/// of them.** `latest-tie-break` reads it as the middle key of DESIGN §3.1's
/// three-key order — *"Greatest `window_end`, then greatest `accepted_at`,
/// then greatest `id` in byte order"* — and two entries stamped from one
/// constant agree on it, so they cannot discriminate on it.
///
/// `gts_type_id` is a parameter for the reason [`check_meter`] gives: a
/// check that selects entries by meter needs a meter no other check writes
/// to.
///
/// The resource, the subject and the metadata stay the shared vocabulary.
/// No check varies them, and holding them fixed is what makes two entries
/// differ exactly where the check that asked for them meant them to.
///
/// The projection derives the entry's `id` and validates the period, so a
/// fixture with an inverted period or a bound finer than a microsecond is
/// refused here rather than reaching a plugin. `Err` carries a
/// ready-to-report detail.
pub fn fixture_record_on(
    gts_type_id: MeterTypeId,
    tenant_id: Uuid,
    idempotency_key: &IdempotencyKey,
    quantity: UsageQuantity,
    accepted_at: time::OffsetDateTime,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<UsageRecord, String> {
    CreateUsageRecord {
        entry_type: EntryType::Record,
        gts_type_id,
        tenant_id,
        resource_ref: ResourceRef::new(CONTRACT_RESOURCE_ID, CONTRACT_RESOURCE_TYPE)
            .map_err(|err| format!("the check's own resource reference is invalid: {err}"))?,
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        quantity,
        idempotency_key: Some(idempotency_key.clone()),
        invalidation: None,
        window_start,
        window_end,
    }
    .try_into_usage_record(RecordOrigin::Live, accepted_at)
    .map_err(|err| format!("the check's own submission is not projectable: {err}"))
}

/// The reason every withdrawal this suite submits states.
///
/// The vocabulary is deliberately open — the gear records the emitter's
/// stated intent and infers nothing from it — so any well-formed code
/// serves and no check reads this one.
const CONTRACT_REASON_CODE: &str = "contract-suite-withdrawal";

/// Builds one faithful invalidation of `target`, under the reason code
/// `CONTRACT_REASON_CODE` names.
///
/// The target is the whole input. A withdrawal repeats every caller-supplied
/// field of its target and departs from it in `entry_type` and the reason
/// code alone (DESIGN §3.1, Faithful copy), so there is nothing left for a
/// caller to supply — and a builder taking loose parts could be handed parts
/// that copy no stored entry.
///
/// Every invalidation of one target therefore repeats that target's
/// idempotency key and carries `entry_type = invalidation`, so all of them
/// share one identity and none shares the target's.
pub fn fixture_invalidation(target: &UsageRecord) -> Result<UsageRecord, String> {
    fixture_invalidation_with_reason(target, CONTRACT_REASON_CODE)
}

/// A [`ContractViolation`] attributed to one check.
///
/// The check name is a parameter rather than baked in. Seven checks report
/// through it and [`HARNESS_FAULT`](super::HARNESS_FAULT) is an eighth
/// caller, and the whole point of [`ContractViolation::check`] is that a
/// violation says which assertion produced it — a helper that stamped one
/// name on every report would quietly undo that.
pub fn violation(check: &'static str, detail: String) -> ContractViolation {
    ContractViolation { check, detail }
}

/// A faithful invalidation of `target` stating `reason`, stamped
/// [`CONTRACT_ACCEPTED_AT`].
///
/// Every invalidation of one target repeats that target's idempotency key and
/// carries `entry_type = invalidation`, so two withdrawals of one target agree
/// on all six identity inputs and derive one id. The reason code is the one
/// field they can differ in, and `at-most-one-invalidation` needs two that do.
///
/// [`fixture_invalidation_on`] is the same builder with the acceptance
/// instant left open, and carries the note on the projection.
pub fn fixture_invalidation_with_reason(
    target: &UsageRecord,
    reason: &str,
) -> Result<UsageRecord, String> {
    fixture_invalidation_on(target, reason, CONTRACT_ACCEPTED_AT)
}

/// The same withdrawal stamped a caller-chosen acceptance instant — the
/// general form [`fixture_invalidation_with_reason`] delegates to.
///
/// `accepted_at` is a parameter for the reason it is one on
/// [`fixture_record_on`]: it is the middle key of DESIGN §3.1's `LATEST`
/// tie-break, and two entries stamped from one constant agree on it.
///
/// The projection is [`CreateUsageRecord::try_into_invalidation_record`]
/// rather than `try_into_usage_record`, which refuses a submission declaring
/// `entry_type = invalidation`: `invalidates` is server-assigned, so the
/// target's `id` is stamped here the way the gateway stamps what it
/// resolved, and the derivation reads the declared `entry_type` instead of
/// the target's.
pub fn fixture_invalidation_on(
    target: &UsageRecord,
    reason: &str,
    accepted_at: time::OffsetDateTime,
) -> Result<UsageRecord, String> {
    let reason = ReasonCode::new(reason)
        .map_err(|err| format!("the check's own reason code is invalid: {err}"))?;
    CreateUsageRecord {
        entry_type: EntryType::Invalidation,
        gts_type_id: target.gts_type_id.clone(),
        tenant_id: target.tenant_id,
        resource_ref: target.resource_ref.clone(),
        subject_ref: target.subject_ref.clone(),
        metadata: target.metadata.clone(),
        quantity: target.quantity,
        idempotency_key: Some(target.idempotency_key.clone()),
        invalidation: Some(reason),
        window_start: target.window_start,
        window_end: target.window_end,
    }
    .try_into_invalidation_record(RecordOrigin::Live, accepted_at, target.id)
    .map_err(|err| format!("the check's own submission is not projectable: {err}"))
}
