//! The vocabulary every check builds its fixtures from.
//!
//! One meter, one tenant and one resource are shared across the whole
//! suite, deliberately: with everything else held fixed, nothing but an
//! entry's covered period can decide it, and each check separates its own
//! entries from every other check's by offsetting them from
//! [`FIXTURE_EPOCH`] — [`check_window_from`] is where every one of those
//! offsets is written down, and where no two of them colliding stops being a
//! matter of inspection. The builders here are the only place a
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

use super::{
    AT_MOST_ONE_INVALIDATION, CONVERGED_TARGET_LOOKUP, ContractViolation, DEDUP_CONCURRENT,
    DEDUP_FLOOR, DEDUP_IDENTITY_OVER_WINDOW, FEED_BOOTSTRAP_POSITION, FEED_COMPLETENESS,
    FEED_POSITION_BOUNDED, FEED_RETENTION_REFUSAL, FEED_SNAPSHOT_AND_REPLAY,
    INVALIDATION_EXCLUDED_FROM_FOLD, LATEST_TIE_BREAK, QUANTITY_ROUND_TRIP,
    RECORD_AND_INVALIDATION_DISTINCT_IDENTITY, SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH,
    SERVER_FIELD_ROUND_TRIP, WINDOW_END_SELECTION,
};
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
/// to go, and it went without anyone having to remember it.
/// [`contract_tenant`] below carried the same shape until
/// [`latest_tie_break`](super::checks::latest_tie_break()) became its first
/// caller, and it retired the same way.
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

/// The tenant the dispatched scope does **not** admit, read by four checks:
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
/// **One reader now removes entries under it.**
/// [`feed_retention_refusal`](super::checks::feed_retention_refusal()) drives
/// a retention sweep that takes an entry of this tenant's, and that is safe
/// for the same reason: [`ContractRetention`](super::retention::ContractRetention)'s
/// drive is keyed on a GTS type, so it reaches only that check's own meter
/// and no other reader's entries.
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
/// [`FEED_POSITION_BOUNDED`]'s rule is about a
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
/// **`feed-position-bounded` is not the first caller after all**, and the
/// dead-code exemption retired itself exactly as [`check_meter`]'s docs said
/// it would. [`latest_tie_break`](super::checks::latest_tie_break()) got here
/// first, for a reason of the same shape: DESIGN §3.3's `latest-tie-break`
/// row asks for *"an aggregate spanning tenants"*, and the `id` key of
/// §3.1's order can only be reached by a group that two tenants' entries
/// both fall in. It mints two, at index 0 and 1.
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
/// **Two callers, one rule between them.** DESIGN §3.1's `LATEST` order ends
/// *"then greatest `id` in byte order"*, so a scenario about either of the
/// two keys above `id` proves nothing unless the entry those keys select is
/// the entry `id` would reject:
/// [`latest_tie_break`](super::checks::latest_tie_break()) needs it twice and
/// `super::contract_tests`'s
/// `latest_breaks_a_window_end_tie_on_the_greater_accepted_at` once, and a
/// single search is what keeps the three from drifting apart.
///
/// The failure is loud rather than silent for the same reason: a scenario
/// built on an inversion that did not happen asserts nothing and reports
/// nothing, which is the one outcome worse than a failing check.
pub fn inverted_id_pair<F>(
    purpose: &str,
    build: F,
) -> Result<(UsageRecord, UsageRecord, u32), String>
where
    F: Fn(u32) -> Result<(UsageRecord, UsageRecord), String>,
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
/// **This is one table rather than seventeen doc comments, and the block
/// below is why.** Each module used to state its own offset and enumerate
/// every other module's to argue it was clear of them. Nothing checked
/// those enumerations, every one of them had to be edited by every check
/// that landed afterwards, and none of them was: seven modules carried a
/// list and all seven were wrong. [`check_window_from`] says what a
/// collision would cost.
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
];

// No two rows of `CHECK_WINDOW_OFFSETS` name one offset, established when
// the crate compiles.
//
// This is the whole reason the offsets are tabled rather than written into
// seventeen doc comments. `super::run_all` dispatches every check against
// one shared, persistent backend that writes entries and never removes
// them, and every fixture shares one meter and one tenant — deliberately,
// so that nothing but the covered period can decide an entry. The offset is
// therefore the only thing keeping one check's entries out of another's
// reads, and two checks sharing one would not fail here: they would fail
// somewhere else, as a count that came back one too high or a fold that
// summed a quantity nobody in that check wrote, and which of the two saw it
// would turn on the order `run_all` happened to dispatch in.
//
// A prose argument cannot hold that, and the prose that used to try did not:
// seven modules carried a list of the offsets taken when they landed, and
// every one of the seven stopped short of the checks that landed after. This
// cannot stop short.
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
/// A check separates its entries from every other check's by offsetting
/// their covered periods from [`FIXTURE_EPOCH`], and
/// [`CHECK_WINDOW_OFFSETS`] is where every one of those offsets is written
/// down. `super::run_all` dispatches every check against one shared backend
/// that writes entries and never removes them, and the shared meter, tenant
/// and resource above are deliberate — with everything else held fixed,
/// nothing but the covered period can decide an entry — so the offset is
/// what keeps one check's entries out of another check's reads, and the
/// other check's out of its.
///
/// **That no two of them collide is established when the crate compiles**,
/// by the block above the table. It is not a matter of inspection, and it
/// was not one that inspection kept.
///
/// `role` separates the windows one check needs from each other, the way it
/// does for [`check_meter`]. A check that needs a second window takes a new
/// row here rather than a corner of its first, so the guard sees it.
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
///
/// It is public because two of the SPI's read paths take the compiled scope
/// as a parameter of their own rather than inside `query.filter` — the feed
/// page and the reconciliation read — so a check reading either takes the
/// expression from here instead of from [`contract_query`].
/// [`server_field_round_trip`](super::checks::server_field_round_trip()) was
/// the first caller, on the feed;
/// [`dedup_concurrent`](super::checks::dedup_concurrent()) is the second,
/// both on the feed and on the point read its `linearizable` half makes.
pub fn contract_scope() -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(CONTRACT_TENANT_ID))),
    )
}

/// `tenant_id eq <first> or tenant_id eq <rest...>`, right-associated.
///
/// This is the shape `authz::scope_to_odata_filter` projects for a grant
/// whose constraint names several tenants, and [`contract_scope`] above is
/// its one-tenant case spelled out. The first tenant is a parameter of its
/// own rather than the head of a slice, so a grant naming none - which would
/// admit nothing and make every assertion resting on it vacuous - cannot be
/// written at all.
///
/// It lives here rather than in a check for the reason
/// [`super::feed_walk`] does: it landed inside
/// [`feed_snapshot_and_replay`](super::checks::feed_snapshot_and_replay()),
/// the first check to need a grant naming more than one tenant, and moved
/// here when the second one needed it.
/// [`feed_position_bounded`](super::checks::feed_position_bounded()) is that
/// second caller, and it needs a grant naming ten.
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
/// The check name is a parameter rather than baked in. Every check in
/// [`super::checks`] reports through it and
/// [`HARNESS_FAULT`](super::HARNESS_FAULT) is one further caller, and the
/// whole point of [`ContractViolation::check`] is that a
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
