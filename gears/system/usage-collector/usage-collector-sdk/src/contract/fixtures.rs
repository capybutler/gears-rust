//! The vocabulary every check builds its fixtures from.
//!
//! One meter, one tenant and one resource are shared across the whole
//! suite, deliberately: with everything else held fixed, nothing but an
//! entry's covered period can decide it, and each check separates its own
//! entries from every other check's by offsetting them from
//! [`FIXTURE_EPOCH`] — [`check_window_from`] is where every one of those
//! offsets is written down, and where no two of them colliding stops being a
//! matter of inspection. The builders here are the only place a
//! [`StoredUsageRecord`] is projected, so two entries differ
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

use super::{
    AT_MOST_ONE_INVALIDATION, CONVERGED_TARGET_LOOKUP, ContractViolation, DEDUP_CONCURRENT,
    DEDUP_FLOOR, DEDUP_IDENTITY_OVER_WINDOW, FEED_BOOTSTRAP_POSITION, FEED_COMPLETENESS,
    FEED_POSITION_BOUNDED, FEED_RETENTION_REFUSAL, FEED_SNAPSHOT_AND_REPLAY,
    INVALIDATION_EXCLUDED_FROM_FOLD, LATEST_TIE_BREAK, QUANTITY_ROUND_TRIP, RAW_PAGE_CALLER_ORDER,
    RAW_PAGE_KEYSET_WALK, RECONCILIATION_FIGURES, RECORD_AND_INVALIDATION_DISTINCT_IDENTITY,
    SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH, SERVER_FIELD_ROUND_TRIP, WINDOW_END_SELECTION,
};
use crate::error::UsageCollectorPluginError;
use crate::models::{
    CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, RECORD_ID_FIELD, ReasonCode,
    RecordOrigin, ResourceRef, USAGE_RECORD_BASE_TYPE, WINDOW_END_FIELD,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::stored::{MeterRef, StoredUsageRecord};

/// The meter the shared builders below attach an entry to. A single derived
/// type is enough for them: no implemented check reads a declaration, and
/// the SPI never sees one — the fold arrives as a parameter.
///
/// A check that cannot share it derives one of its own through
/// [`check_meter`].
pub const CONTRACT_METER_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.core.uc.contract_suite.v1~");

/// The registry reference [`contract_meter`] pairs with [`CONTRACT_METER_TYPE_ID`].
///
/// Picked by hand rather than derived: deriving it here would be a second call
/// to `GtsId::to_uuid()` in this crate, and [`check_meter`]'s own doc names
/// itself the only site permitted that call.
///
/// **Load-bearing.** [`super::reference`] keys its ledger on the reference a
/// [`StoredUsageRecord`] carries, so this value is what separates the shared
/// meter's entries from every [`check_meter`]-derived meter's; a value a
/// check's own meter could collide with would merge two ledgers silently.
///
/// It cannot collide. Every reference [`check_meter`] mints takes
/// `GtsId::to_uuid()`'s `UUIDv5` branch, because the identifier's last segment
/// is never a UUID, so all of them carry version nibble `5`. The literal below
/// is version `4`, and the assertion under it says so when the crate
/// compiles.
const CONTRACT_METER_TYPE_UUID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_c047_c047);

// The shared meter's reference is not a `UUIDv5`, established when the crate
// compiles. A collision would not fail loudly: it would merge one check's
// entries into another's reference-keyed ledger and turn what either observed
// into a matter of dispatch order.
const _: () = assert!(
    CONTRACT_METER_TYPE_UUID.as_u128() >> 76 & 0xf != 5,
    "the shared contract meter's reference is version 5, which is what \
     `check_meter`'s `GtsId::to_uuid()` derivation mints: a check deriving \
     this very value would write its entries into the shared meter's ledger"
);

/// A [`MeterRef`] for the whole suite's shared [`CONTRACT_METER_TYPE_ID`]
/// meter, for a check that neither needs nor mints one of its own through
/// [`check_meter`].
pub fn contract_meter() -> Result<MeterRef, String> {
    Ok(MeterRef::new(
        CONTRACT_METER_TYPE_UUID,
        MeterTypeId::new(CONTRACT_METER_TYPE_ID)
            .map_err(|err| format!("the shared contract meter id is invalid: {err}"))?,
    ))
}

/// Derives a meter for one check's exclusive use.
///
/// [`CONTRACT_METER_TYPE_ID`] is shared by the whole suite, and a check that
/// selects entries by meter — a feed read, whose subscription is a list of
/// meters — cannot live with that: `super::run_all` dispatches every check
/// against one persistent backend that never removes entries, so a page read
/// over the shared meter carries whatever the other checks left on it. A meter
/// derived here is written and read by one check alone.
///
/// `role` separates the meters one check needs from each other — a check that
/// subscribes to one meter and deliberately writes to a second, say.
///
/// **Collision-freedom is structural rather than asserted.** Check names are
/// unique, held so by
/// `the_three_coverage_constants_partition_the_design_checks`, so two
/// derivations collide only when one check passes one `role` twice.
///
/// Every `-` in the pair becomes `_`, because the `gts-id` grammar admits only
/// `[a-z0-9_]` inside a token and every DESIGN §3.3 check name is hyphenated.
///
/// `Err` carries a ready-to-report detail, but
/// `every_check_name_derives_a_distinct_valid_meter` asserts that every check
/// name derives at all, so the failure cannot reach a plugin author as a
/// harness fault against a conforming backend.
///
/// **This is also the suite's only reference-minting site**, and the one place
/// in this crate permitted to call `GtsId::to_uuid()`. Production code is
/// barred from the derivation — the gear takes the reference off the resolved
/// `GtsTypeSchema`, and a plugin takes it off the `MeterRef` it is handed — so
/// a *fixture* that derives is one whose job is to fail if the derivation ever
/// moves under a `gts` upgrade.
pub fn check_meter(check: &str, role: &str) -> Result<MeterRef, String> {
    let slug = format!("{check}_{role}").replace('-', "_");
    let raw = format!("{USAGE_RECORD_BASE_TYPE}cf.core.uc.{slug}.v1~");
    let id = MeterTypeId::new(&raw)
        .map_err(|err| format!("the check's own derived meter id `{slug}` is invalid: {err}"))?;
    let uuid = gts::GtsId::try_new(&raw)
        .map_err(|err| format!("the check's own derived meter id `{slug}` is not a GTS id: {err}"))?
        .to_uuid();
    Ok(MeterRef::new(uuid, id))
}

/// The tenant every fixture entry is attributed to, and the one value the
/// scope filter the suite dispatches pins.
///
/// First of the suite's tenant ids. They are all minted in this module, and
/// the assertion under [`FEED_OTHER_TENANT_ID`] says why.
pub const CONTRACT_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0001);

/// The tenant the dispatched scope does **not** admit, read by these checks:
/// [`scope_is_a_filter_on_every_read_path`](super::checks::scope_is_a_filter_on_every_read_path()),
/// [`converged_target_lookup`](super::checks::converged_target_lookup()),
/// [`feed_snapshot_and_replay`](super::checks::feed_snapshot_and_replay())
/// and
/// [`feed_retention_refusal`](super::checks::feed_retention_refusal()). Each
/// module says what it is for. Several readers are safe here where two would
/// not be under [`SCOPE_UNUSED_TENANT_ID`] below, and the difference is what
/// each asserts: nothing here turns on how many entries this tenant owns,
/// because every reader names an entry of its own and requires that one
/// withheld. So a further reader adds an entry rather than dissolving a
/// premise. Each names its own by `id` on the point read and keeps its
/// entries off the others' collection reads through [`check_meter`].
///
/// One reader removes entries under it:
/// [`feed_retention_refusal`](super::checks::feed_retention_refusal()) drives
/// a retention sweep that takes one. That is safe for the same reason —
/// [`ContractRetention`](super::retention::ContractRetention)'s drive is keyed
/// on a GTS type, so it reaches only that check's own meter.
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
// The ids are minted here rather than in each module that reads them because
// `super::run_all` dispatches every check against one shared, persistent
// backend that never removes an entry, so one check's fixtures are visible to
// the next — and `SCOPE_UNUSED_TENANT_ID`'s entire assertion value is that it
// owns no entry. Two checks minting one id dissolves that premise silently
// rather than failing anything, which a doc comment cannot hold and this can.
//
// `contract_tenant` reaches the same premise by another route: it is a factory,
// so nothing about its call sites says which ids it hands out. The second
// assertion keeps its block clear of the named ids, so the factory cannot hand
// a check `SCOPE_UNUSED_TENANT_ID`.
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
/// [`FEED_POSITION_BOUNDED`]'s rule is about a subscription spanning **many**
/// tenants — DESIGN §3.3 gives it as *"A position issued for a subscription
/// spanning many tenants encodes to the same size as one spanning few"* — so a
/// check asserting it needs more tenants than the named ones above, without
/// colliding with them. That the block is clear of them is a compile-time
/// assertion rather than a matter of inspection.
///
/// [`latest_tie_break`](super::checks::latest_tie_break()) is a caller for a
/// reason of the same shape: DESIGN §3.3's `latest-tie-break` row asks for *"an
/// aggregate spanning tenants"*, and §3.1's `id` key can only be reached by a
/// group two tenants' entries both fall in.
#[must_use]
pub fn contract_tenant(index: u32) -> Uuid {
    Uuid::from_u128(CONTRACT_TENANT_BLOCK + u128::from(index))
}

/// How many indexed idempotency keys [`inverted_id_pair`] tries before it
/// gives up.
///
/// Each attempt inverts with probability one half and the attempts are
/// independent, so sixty-four of them miss with probability 2^-64. The bound
/// is there so a derivation that stopped producing an inversion at all fails
/// loudly instead of looping.
const ID_INVERSION_ATTEMPTS: u32 = 64;

/// Searches indexed idempotency keys for a pair of entries whose **winner**
/// carries the smaller `id`.
///
/// `build` is handed an attempt index and returns the pair `(winner, loser)`
/// the caller wants for that index — "winner" meaning the entry the rule
/// under test is required to select. The search stops at the first index
/// where `winner.id < loser.id` and returns the pair alongside that index;
/// `Err` carries a ready-to-report detail naming `purpose`, which the caller
/// supplies to say what its own scenario needed the inversion for.
///
/// **The inversion cannot be chosen, which is why this is a search.** An
/// `id` is the `UUIDv5` over the six identity inputs
/// ([`derive_usage_record_id`](crate::id::derive_usage_record_id)), so which
/// of two entries carries the greater one is a property of the derived
/// values. The one input a fixture is free to vary without changing what the
/// entry means is the idempotency key, and the index this varies is part of
/// that key.
///
/// **Why callers need it.** DESIGN §3.1's `LATEST` order ends *"then greatest
/// `id` in byte order"*, so a scenario about either key above `id` proves
/// nothing unless the entry those keys select is the entry `id` would reject.
/// One search keeps every such scenario from drifting apart, and the failure is
/// loud because a scenario built on an inversion that did not happen asserts
/// nothing and reports nothing.
pub fn inverted_id_pair<F>(
    purpose: &str,
    build: F,
) -> Result<(StoredUsageRecord, StoredUsageRecord, u32), String>
where
    F: Fn(u32) -> Result<(StoredUsageRecord, StoredUsageRecord), String>,
{
    for attempt in 0..ID_INVERSION_ATTEMPTS {
        let (winner, loser) = build(attempt)?;
        if winner.id < loser.id {
            return Ok((winner, loser, attempt));
        }
    }
    Err(format!(
        "no pair of indexed idempotency keys put the winning entry under the smaller `id` \
         within {ID_INVERSION_ATTEMPTS} attempts. {purpose} An `id` is the UUIDv5 over the six \
         identity inputs, so which of two entries carries the greater one is a property of the \
         derived values rather than something a fixture chooses, and the search varies the one \
         input that is free here - the idempotency key - rather than picking a pair."
    ))
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

/// Where each check's fixture window sits, as whole days past
/// [`FIXTURE_EPOCH`].
///
/// One row per `(check, role)`, mirroring [`check_meter`]'s shape and for
/// the same reason: a check can need more than one, and
/// [`INVALIDATION_EXCLUDED_FROM_FOLD`] does — one window carrying a
/// withdrawn pair with a live entry beside it, and one carrying nothing but
/// a withdrawn pair, because what it asserts over the second means nothing
/// if anything survives there.
///
/// The check is named by its constant rather than spelled as a literal, so
/// a check renamed out from under this table is a compile error rather than
/// a row that quietly matches nothing.
///
/// **One table rather than a doc comment per check module**, so that no two
/// offsets colliding is the compile-time assertion below rather than a matter
/// of inspection. [`check_window_from`] says what a collision would cost.
const CHECK_WINDOW_OFFSETS: &[(&str, &str, i64)] = &[
    (QUANTITY_ROUND_TRIP, "main", 0),
    (WINDOW_END_SELECTION, "main", 30),
    (DEDUP_IDENTITY_OVER_WINDOW, "main", 60),
    (INVALIDATION_EXCLUDED_FROM_FOLD, "main", 90),
    (AT_MOST_ONE_INVALIDATION, "main", 120),
    (SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH, "main", 150),
    (RECORD_AND_INVALIDATION_DISTINCT_IDENTITY, "main", 180),
    (SERVER_FIELD_ROUND_TRIP, "main", 210),
    (DEDUP_FLOOR, "main", 240),
    (CONVERGED_TARGET_LOOKUP, "main", 270),
    (DEDUP_CONCURRENT, "main", 300),
    (LATEST_TIE_BREAK, "main", 330),
    (FEED_SNAPSHOT_AND_REPLAY, "main", 360),
    (FEED_COMPLETENESS, "main", 390),
    (FEED_BOOTSTRAP_POSITION, "main", 420),
    (FEED_RETENTION_REFUSAL, "main", 450),
    (FEED_POSITION_BOUNDED, "main", 480),
    (INVALIDATION_EXCLUDED_FROM_FOLD, "empty", 510),
    (RECONCILIATION_FIGURES, "main", 540),
    (RAW_PAGE_KEYSET_WALK, "main", 570),
    (RAW_PAGE_KEYSET_WALK, "ties", 571),
    (RAW_PAGE_KEYSET_WALK, "look_ahead", 572),
    (RAW_PAGE_CALLER_ORDER, "main", 573),
];

// No two rows of `CHECK_WINDOW_OFFSETS` name one offset, established when the
// crate compiles.
//
// `super::run_all` dispatches every check against one shared, persistent
// backend that never removes an entry, and every fixture shares one meter and
// one tenant — deliberately, so nothing but the covered period can decide an
// entry. The offset is therefore the only thing keeping one check's entries out
// of another's reads, and two checks sharing one would not fail here: it would
// surface elsewhere as a count one too high or a fold over a quantity nobody in
// that check wrote, and which of the two saw it would turn on dispatch order.
const _: () = {
    let mut first = 0;
    while first < CHECK_WINDOW_OFFSETS.len() {
        let mut second = first + 1;
        while second < CHECK_WINDOW_OFFSETS.len() {
            assert!(
                CHECK_WINDOW_OFFSETS[first].2 != CHECK_WINDOW_OFFSETS[second].2,
                "two of the contract suite's check windows sit at one day offset past \
                 `FIXTURE_EPOCH`: the suite runs every check against one shared backend that \
                 never removes an entry, and every fixture shares one meter and one tenant, so \
                 two checks on one window read each other's entries and what either observes \
                 turns on dispatch order"
            );
            second += 1;
        }
        first += 1;
    }
};

/// Whether two strings hold the same bytes.
///
/// `str`'s own `PartialEq` is not callable in a `const fn` on stable, and
/// [`check_window_from`] is reached from the `const` every check module
/// declares its window as, so the comparison is spelled out here.
const fn same_name(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// The instant one check's fixture window begins.
///
/// A check separates its entries from every other check's by offsetting their
/// covered periods from [`FIXTURE_EPOCH`], and [`CHECK_WINDOW_OFFSETS`] is
/// where every one of those offsets is written down. With the shared meter,
/// tenant and resource held fixed, the offset is what keeps one check's entries
/// out of another's reads — and that no two collide is established when the
/// crate compiles, by the block above the table.
///
/// `role` separates the windows one check needs from each other, as it does for
/// [`check_meter`]. A check that needs a second window takes a new row here
/// rather than a corner of its first, so the guard sees it.
///
/// # Panics
///
/// When `(check, role)` has no row. Every caller is a `const`, so an
/// untabled pair fails the build at the constant that asked for it.
pub const fn check_window_from(check: &str, role: &str) -> time::OffsetDateTime {
    let mut index = 0;
    while index < CHECK_WINDOW_OFFSETS.len() {
        let (tabled_check, tabled_role, days) = CHECK_WINDOW_OFFSETS[index];
        if same_name(tabled_check, check) && same_name(tabled_role, role) {
            return FIXTURE_EPOCH.saturating_add(time::Duration::days(days));
        }
        index += 1;
    }
    panic!(
        "no row of `CHECK_WINDOW_OFFSETS` gives this check and role a fixture window; add one \
         rather than reusing another check's"
    )
}

/// The acceptance instant every fixture entry carries. Fixed rather than read
/// from the clock so a check's expected entries compare equal to what it
/// wrote; no slice-A check varies it.
pub const CONTRACT_ACCEPTED_AT: time::OffsetDateTime =
    time::OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::days(20_454));

/// The resource every fixture entry is attributed to. `resource_ref` is
/// mandatory on a [`StoredUsageRecord`] and no implemented check varies it.
pub const CONTRACT_RESOURCE_ID: &str = "contract-suite-resource";
/// The resource-type discriminator paired with [`CONTRACT_RESOURCE_ID`].
pub const CONTRACT_RESOURCE_TYPE: &str = "contract.suite";

/// The `filter_hash` the suite dispatches with.
///
/// `UsageCollectorPluginV1::list_usage_records` states the slot "carries no
/// guarantee on this method and MUST NOT be read". The suite still dispatches a
/// non-`None` value, purely for shape coverage: a plugin that read the slot
/// despite the prohibition meets a populated one here rather than the `None` an
/// in-process caller would send.
const CONTRACT_FILTER_HASH: &str = "usage-collector-contract-suite";

/// The compiled PDP scope the suite dispatches: `tenant_id eq <tenant>`.
///
/// This is the shape `authz::scope_to_odata_filter` projects for a
/// single-tenant grant whose constraint carries one filter: a bare
/// `Compare`, since the projection only builds a conjunction once there is
/// a second filter to AND.
///
/// What dispatching it buys is **shape coverage** — a plugin that chokes on a
/// filter, or ignores `query.filter` and so never exercises its projection,
/// meets one here. It buys no scope *enforcement*: every fixture the other
/// checks build belongs to the tenant this filter pins. Enforcement is
/// [`scope_is_a_filter_on_every_read_path`](super::checks::scope_is_a_filter_on_every_read_path())'s
/// job, and it dispatches a scope of its own through
/// [`contract_query_with_scope`].
///
/// Public because the feed page and the reconciliation read take the compiled
/// scope as a parameter of their own rather than inside `query.filter`, so a
/// check reading either takes the expression from here instead of from
/// [`contract_query`].
pub fn contract_scope() -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(CONTRACT_TENANT_ID))),
    )
}

/// `tenant_id eq <first> or tenant_id eq <rest...>`, right-associated.
///
/// This is the shape `authz::scope_to_odata_filter` projects for a grant whose
/// constraint names several tenants; [`contract_scope`] above is its one-tenant
/// case. The first tenant is a parameter of its own rather than the head of a
/// slice, so a grant naming none — which would admit nothing and make every
/// assertion resting on it vacuous — cannot be written at all.
pub fn tenant_disjunction(first: Uuid, rest: &[Uuid]) -> ast::Expr {
    let named = |tenant_id: Uuid| {
        ast::Expr::Compare(
            Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::Uuid(tenant_id))),
        )
    };
    rest.iter().rev().fold(named(first), |grant, tenant_id| {
        ast::Expr::Or(Box::new(grant), Box::new(named(*tenant_id)))
    })
}

/// The query the suite's read paths dispatch: the compiled scope as the
/// filter, the canonical `(window_end, id)` keyset as the order, and
/// [`CONTRACT_FILTER_HASH`] on the `filter_hash` slot — for shape coverage,
/// not because a real gateway dispatch populates it; see that constant's
/// doc.
pub fn contract_query(limit: u64) -> ODataQuery {
    contract_query_with_scope(limit, contract_scope())
}

/// The same query under a caller-supplied compiled scope.
///
/// The gateway ANDs the compiled scope into `query.filter` before dispatch, so
/// a plugin reads a scope on the collection paths through that slot and nowhere
/// else. A check asserting the scope is *enforced* rather than merely accepted
/// has to put an expression there that admits some stored rows and withholds
/// others, which [`contract_query`]'s single-tenant filter cannot do.
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
) -> Result<StoredUsageRecord, String> {
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
/// A read that gates on the scope has nothing to assert unless two stored
/// entries fall on opposite sides of the scope it dispatches, which is what
/// [`scope_is_a_filter_on_every_read_path`](super::checks::scope_is_a_filter_on_every_read_path())
/// and `super::contract_tests`'s `feed_ledger` need.
///
/// `tenant_id` is a derived-identity input
/// ([`derive_usage_record_id`](crate::id::derive_usage_record_id)), so two
/// entries differing only here are two entries rather than a replay of one.
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
) -> Result<StoredUsageRecord, String> {
    fixture_record_on(
        &contract_meter()?,
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
/// **`accepted_at` is a parameter here and [`CONTRACT_ACCEPTED_AT`] in both of
/// them**, because `latest-tie-break` reads it as the middle key of DESIGN
/// §3.1's order and two entries stamped from one constant cannot discriminate
/// on it. `meter` is a parameter for the reason [`check_meter`] gives. The
/// resource, the subject and the metadata stay the shared vocabulary, so two
/// entries differ exactly where the check meant them to.
///
/// The projection derives the entry's `id` and validates the period, so a
/// fixture with an inverted period or a bound finer than a microsecond is
/// refused here rather than reaching a plugin. `Err` carries a
/// ready-to-report detail.
pub fn fixture_record_on(
    meter: &MeterRef,
    tenant_id: Uuid,
    idempotency_key: &IdempotencyKey,
    quantity: UsageQuantity,
    accepted_at: time::OffsetDateTime,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<StoredUsageRecord, String> {
    // Built as a `UsageRecord` first, so the entry identity goes on through
    // `try_into_usage_record` over `meter.id` — the identifier — and the
    // reference is attached afterwards. A `StoredUsageRecord` assembled field
    // by field here would be the one place the derivation could quietly stop
    // reading the identifier.
    let record = CreateUsageRecord {
        entry_type: EntryType::Record,
        gts_type_id: meter.id.clone(),
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
    .map_err(|err| format!("the check's own submission is not projectable: {err}"))?;
    Ok(record.into_stored(meter.uuid))
}

/// Persist one entry through the batch SPI, with the shape the retired
/// single-entry SPI had.
///
/// Seeding is not the thing under test, so this helper exists to make a seeding
/// site one line rather than an unwrap chain. The outer `Err` is a request-wide
/// refusal; slot 0's own `Result` is the per-record outcome, returned
/// unchanged.
///
/// **Not for checks whose subject is dedup resolution.** Two entries seeded
/// through one call are resolved against each other by the batch SPI's
/// intra-batch rule; two entries seeded through two calls are resolved by
/// the cross-call dedup rule. Those are different contracts. A check that
/// means the second must make two separate calls — see
/// `dedup_concurrent` and `at_most_one_invalidation`.
///
/// # Panics
///
/// Never. `create_usage_records` returns one outcome per entry, aligned
/// with the input order (its own doc comment), and this call hands it
/// exactly one entry, so the single slot is always present.
///
/// # Errors
///
/// The request-wide failure of the underlying
/// [`UsageCollectorPluginV1::create_usage_records`] call, or the per-record
/// error in its single slot.
pub async fn seed_usage_record(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterRef,
    record: StoredUsageRecord,
) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
    let mut outcomes = plugin
        .create_usage_records(vec![(meter.clone(), record)])
        .await?
        .into_iter();
    // Split into a separate binding to scope the `expect_used` suppression to
    // it alone, not the whole function: one entry in is always one slot out
    // (`create_usage_records`'s per-record-outcomes guarantee), so the `None`
    // arm is unreachable rather than a caller-visible outcome.
    #[allow(clippy::expect_used)]
    let outcome = outcomes.next().expect("one entry in, one slot out");
    outcome
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
/// field of its target and departs from it in `entry_type` and the reason code
/// alone (DESIGN §3.1, Faithful copy), so there is nothing left for a caller to
/// supply — and a builder taking loose parts could be handed parts that copy no
/// stored entry. Every invalidation of one target therefore shares one identity
/// and none shares the target's.
///
/// **The meter rides alongside the target** because `meter.id` is a
/// derived-identity input and a [`StoredUsageRecord`] names its meter by
/// reference alone.
pub fn fixture_invalidation(
    meter: &MeterRef,
    target: &StoredUsageRecord,
) -> Result<StoredUsageRecord, String> {
    fixture_invalidation_with_reason(meter, target, CONTRACT_REASON_CODE)
}

/// A [`ContractViolation`] attributed to one check.
///
/// The check name is a parameter rather than baked in: the point of
/// [`ContractViolation::check`] is that a violation says which assertion
/// produced it, which a helper stamping one name on every report would undo.
pub fn violation(check: &'static str, detail: String) -> ContractViolation {
    ContractViolation { check, detail }
}

/// A faithful invalidation of `target` stating `reason`, stamped
/// [`CONTRACT_ACCEPTED_AT`].
///
/// Two withdrawals of one target agree on every identity input and derive one
/// id, so the reason code is the one field they can differ in, which
/// `at-most-one-invalidation` needs.
///
/// [`fixture_invalidation_on`] is the same builder with the acceptance
/// instant left open, and carries the note on the projection.
pub fn fixture_invalidation_with_reason(
    meter: &MeterRef,
    target: &StoredUsageRecord,
    reason: &str,
) -> Result<StoredUsageRecord, String> {
    fixture_invalidation_on(meter, target, reason, CONTRACT_ACCEPTED_AT)
}

/// The same withdrawal stamped a caller-chosen acceptance instant — the
/// general form [`fixture_invalidation_with_reason`] delegates to.
///
/// `accepted_at` is a parameter for [`fixture_record_on`]'s reason.
///
/// The projection is [`CreateUsageRecord::try_into_invalidation_record`] rather
/// than `try_into_usage_record`, which refuses a submission declaring
/// `entry_type = invalidation`: `invalidates` is server-assigned, so the
/// target's `id` is stamped here the way the gateway stamps what it resolved.
pub fn fixture_invalidation_on(
    meter: &MeterRef,
    target: &StoredUsageRecord,
    reason: &str,
    accepted_at: time::OffsetDateTime,
) -> Result<StoredUsageRecord, String> {
    let reason = ReasonCode::new(reason)
        .map_err(|err| format!("the check's own reason code is invalid: {err}"))?;
    // Built then converted, for [`fixture_record_on`]'s reason: the
    // withdrawal's own identity is derived over `meter.id`, and the reference
    // is attached once the derivation has run.
    let record = CreateUsageRecord {
        entry_type: EntryType::Invalidation,
        gts_type_id: meter.id.clone(),
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
    .map_err(|err| format!("the check's own submission is not projectable: {err}"))?;
    Ok(record.into_stored(meter.uuid))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod seed_usage_record_tests {
    use async_trait::async_trait;
    use uuid::Uuid;

    use super::{FIXTURE_EPOCH, contract_meter, fixture_record, seed_usage_record};
    use crate::error::UsageCollectorPluginError;
    use crate::feed::{FeedPage, FeedPosition, FeedStart};
    use crate::keyset::{Keyset, RecordPage};
    use crate::models::{
        AggregationDimension, AggregationFold, AggregationResult, IdempotencyKey, MetadataFilter,
    };
    use crate::plugin_api::UsageCollectorPluginV1;
    use crate::quantity::UsageQuantity;
    use crate::reconciliation::ReconciliationMetadata;
    use crate::stored::{MeterRef, StoredUsageRecord};
    use crate::time_range::TimeRange;

    /// Refuses every submission outright — the outer `Err` channel of
    /// `create_usage_records`: the backend could not accept the request at
    /// all, as distinct from accepting it and rejecting an entry inside it.
    ///
    /// Only `create_usage_records` is exercised by `seed_usage_record`; every
    /// other method panics, so a future caller of this stub outside its
    /// intended use is caught rather than served a made-up answer.
    struct RefusingPlugin;

    impl RefusingPlugin {
        fn new() -> Self {
            Self
        }
    }

    #[async_trait]
    impl UsageCollectorPluginV1 for RefusingPlugin {
        async fn create_usage_records(
            &self,
            _records: Vec<(MeterRef, StoredUsageRecord)>,
        ) -> Result<
            Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>,
            UsageCollectorPluginError,
        > {
            Err(UsageCollectorPluginError::internal(
                "RefusingPlugin refuses every submission outright",
            ))
        }

        async fn get_usage_record(
            &self,
            _id: Uuid,
            _scope: &toolkit_odata::ast::Expr,
            _converged_only: bool,
        ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }

        async fn query_aggregated_usage_records(
            &self,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _fold: AggregationFold,
            _query: &toolkit_odata::ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _group_by: &[AggregationDimension],
        ) -> Result<AggregationResult, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }

        async fn list_usage_records(
            &self,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _query: &toolkit_odata::ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _keyset: Option<&Keyset>,
        ) -> Result<RecordPage, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }

        async fn read_feed_page(
            &self,
            _subscription: &[MeterRef],
            _scope: &toolkit_odata::ast::Expr,
            _start: FeedStart<FeedPosition>,
            _until: Option<FeedPosition>,
            _limit: u64,
        ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }

        async fn get_reconciliation_metadata(
            &self,
            _tenant_id: Uuid,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _fold: AggregationFold,
            _scope: &toolkit_odata::ast::Expr,
        ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }
    }

    /// Accepts the request but rejects the one record inside it — the inner
    /// `Err` channel, carried in slot 0 rather than in the outer `Result`.
    ///
    /// Same panic-on-anything-else discipline as [`RefusingPlugin`], and for
    /// the same reason.
    struct PerRecordRejectingPlugin;

    impl PerRecordRejectingPlugin {
        fn new() -> Self {
            Self
        }
    }

    #[async_trait]
    impl UsageCollectorPluginV1 for PerRecordRejectingPlugin {
        async fn create_usage_records(
            &self,
            records: Vec<(MeterRef, StoredUsageRecord)>,
        ) -> Result<
            Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>,
            UsageCollectorPluginError,
        > {
            Ok(records
                .into_iter()
                .map(|_| {
                    Err(UsageCollectorPluginError::internal(
                        "PerRecordRejectingPlugin rejects every record handed to it",
                    ))
                })
                .collect())
        }

        async fn get_usage_record(
            &self,
            _id: Uuid,
            _scope: &toolkit_odata::ast::Expr,
            _converged_only: bool,
        ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }

        async fn query_aggregated_usage_records(
            &self,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _fold: AggregationFold,
            _query: &toolkit_odata::ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _group_by: &[AggregationDimension],
        ) -> Result<AggregationResult, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }

        async fn list_usage_records(
            &self,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _query: &toolkit_odata::ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _keyset: Option<&Keyset>,
        ) -> Result<RecordPage, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }

        async fn read_feed_page(
            &self,
            _subscription: &[MeterRef],
            _scope: &toolkit_odata::ast::Expr,
            _start: FeedStart<FeedPosition>,
            _until: Option<FeedPosition>,
            _limit: u64,
        ) -> Result<FeedPage<FeedPosition, StoredUsageRecord>, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }

        async fn get_reconciliation_metadata(
            &self,
            _tenant_id: Uuid,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _fold: AggregationFold,
            _scope: &toolkit_odata::ast::Expr,
        ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
            unimplemented!("not exercised by seed_usage_record")
        }
    }

    /// One fixture entry, built the way this file's other tests build one
    /// (see `reference_sort_key_tests` in `reference.rs`): a fresh
    /// idempotency key and an arbitrary, valid one-hour window. Neither
    /// test below reads these values — they exist only so the plugin has a
    /// record to answer about — so there is no fixture-window-table row to
    /// reserve for this module: nothing here ever reaches the shared
    /// contract backend `CHECK_WINDOW_OFFSETS` protects.
    fn seed_fixture_record() -> StoredUsageRecord {
        let idempotency_key = IdempotencyKey::new("seed-usage-record-helper-test")
            .expect("the test's own idempotency key is valid");
        let window_start = FIXTURE_EPOCH;
        let window_end = FIXTURE_EPOCH.saturating_add(time::Duration::hours(1));
        fixture_record(
            &idempotency_key,
            UsageQuantity::parse("1").expect("1 is inside the published range"),
            window_start,
            window_end,
        )
        .expect("the test's own fixture is well formed")
    }

    #[tokio::test]
    async fn seed_usage_record_surfaces_a_request_wide_refusal() {
        // The outer Err channel: the backend refused the submission as a whole.
        let plugin = RefusingPlugin::new();
        let meter = contract_meter().expect("the suite's own meter is valid");
        let record = seed_fixture_record();

        let outcome = seed_usage_record(&plugin, &meter, record).await;

        assert!(
            outcome.is_err(),
            "a request-wide refusal must reach the caller, not be read as an empty slot",
        );
    }

    #[tokio::test]
    async fn seed_usage_record_surfaces_a_per_record_rejection() {
        // The inner Err channel: the submission was accepted, the entry was not.
        let plugin = PerRecordRejectingPlugin::new();
        let meter = contract_meter().expect("the suite's own meter is valid");
        let record = seed_fixture_record();

        let outcome = seed_usage_record(&plugin, &meter, record).await;

        assert!(
            outcome.is_err(),
            "a per-record rejection in slot 0 must reach the caller unchanged",
        );
    }
}
