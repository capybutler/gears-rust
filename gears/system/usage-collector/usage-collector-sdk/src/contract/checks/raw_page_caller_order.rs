//! The `raw-page-caller-order` check.
//!
//! See [`raw_page_caller_order`] for what it asserts. The SPI's
//! `list_usage_records` doc states the obligation this check makes testable:
//! *"`query.order` MUST be honoured … Those two names are guaranteed to be
//! *present*, not to be last: a caller ordering by `id` is handed on as
//! `(id, window_end)`. A plugin MUST read the order it is given rather than
//! assume a position for either key."* Only a structured
//! [`crate::keyset::Keyset`] can be inspected to tell a conforming plugin from
//! one that assumes a fixed position for either canonical field.

use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};
use uuid::Uuid;

use crate::contract::fixtures::{
    CONTRACT_ACCEPTED_AT, check_window_from, contract_meter, contract_tenant, fixture_record_on,
    seed_usage_record, violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, RAW_PAGE_CALLER_ORDER};
use crate::keyset::Keyset;
use crate::models::IdempotencyKey;
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::stored::{MeterRef, StoredUsageRecord};
use crate::time_range::TimeRange;

/// A plugin whose keyset does not advance would loop forever; see
/// `raw_page_keyset_walk`'s identical constant for the same reasoning.
const MAX_PAGES: usize = 32;

/// The page limit dispatched over this check's four entries: two pages,
/// one continuation.
const PAGE_LIMIT: u64 = 2;

/// `raw-page-caller-order` — a caller-supplied order is honoured, survives
/// a continuation, and shapes the returned keyset.
///
/// 1. Dispatched with an order of `(tenant_id, window_end, id)` ascending,
///    the page's entries are non-decreasing in that tuple.
/// 2. Paged at [`PAGE_LIMIT`], the concatenation of the pages equals **both**
///    the fixture's own independent `(tenant_id, window_end)` ordering
///    (`caller_order_fixtures`'s layout table, never a plugin call) **and**
///    the single-call result read under the same order — comparing against
///    the fixture closes what comparing only against a second plugin call
///    would miss: a plugin applying one consistent wrong order to both
///    reads.
/// 3. The returned [`Keyset`] carries one value per key of the dispatched
///    order and that order's single direction. This is the assertion a
///    plugin reading a fixed position for `window_end` or `id` fails: the
///    dispatched order here leads with `tenant_id`, which neither
///    canonical field is. A sibling walk under the same order **descending**
///    pins the direction half, which no ascending-only dispatch can: every
///    order this suite otherwise sends is ascending, so a plugin hardcoding
///    [`SortDir::Asc`] into every keyset it mints would pass unnoticed
///    without it.
pub async fn raw_page_caller_order(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let fixtures = match caller_order_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{RAW_PAGE_CALLER_ORDER}` \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in \
                     the plugin under test: {detail}"
                ),
            )];
        }
    };

    for record in &fixtures.records {
        if let Err(err) = seed_usage_record(plugin, &fixtures.meter, record.clone()).await {
            return vec![violation(
                RAW_PAGE_CALLER_ORDER,
                format!(
                    "`create_usage_records` refused a caller-order fixture (record {id}): {err}",
                    id = record.id,
                ),
            )];
        }
    }

    let query = caller_order_query(PAGE_LIMIT, SortDir::Asc);
    let pages = match walk_with_keysets(plugin, &fixtures.meter, fixtures.range, &query).await {
        Ok(pages) => pages,
        Err(detail) => return vec![violation(RAW_PAGE_CALLER_ORDER, detail)],
    };

    let single_call_query = caller_order_query(
        u64::try_from(fixtures.records.len() + 1).unwrap_or(u64::MAX),
        SortDir::Asc,
    );
    let single = match plugin
        .list_usage_records(
            &fixtures.meter,
            fixtures.range,
            &single_call_query,
            &[],
            None,
        )
        .await
    {
        Ok(page) => page,
        Err(err) => {
            return vec![violation(
                RAW_PAGE_CALLER_ORDER,
                format!(
                    "`list_usage_records` failed reading this check's range in one unpaginated \
                     call, so the paginated walk had nothing to be compared against: {err}"
                ),
            )];
        }
    };

    let mut violations = Vec::new();
    let delivered: Vec<StoredUsageRecord> =
        pages.iter().flat_map(|(items, _)| items.clone()).collect();

    // Assertion 1: non-decreasing in (tenant_id, window_end, id).
    for pair in delivered.windows(2) {
        let (left, right) = (&pair[0], &pair[1]);
        let left_key = (left.tenant_id, left.window_end, left.id);
        let right_key = (right.tenant_id, right.window_end, right.id);
        if left_key > right_key {
            violations.push(violation(
                RAW_PAGE_CALLER_ORDER,
                format!(
                    "record {left_id} (tenant {left_tenant}, window_end {left_end}) was \
                     delivered immediately before record {right_id} (tenant {right_tenant}, \
                     window_end {right_end}), which is not non-decreasing in the dispatched \
                     order `(tenant_id, window_end, id)`.",
                    left_id = left.id,
                    left_tenant = left.tenant_id,
                    left_end = left.window_end,
                    right_id = right.id,
                    right_tenant = right.tenant_id,
                    right_end = right.window_end,
                ),
            ));
        }
    }

    // Assertion 2a: the walk matches the fixture's own independent expectation
    // — `caller_order_fixtures`'s layout table, never a call to the plugin under
    // test. Comparing only against the single-call read below would let a plugin
    // applying one consistent wrong order to both pass, and would not catch a
    // plugin silently dropping or duplicating a row identically on both calls.
    let delivered_ids: Vec<Uuid> = delivered.iter().map(|record| record.id).collect();
    if delivered_ids != fixtures.ascending_ids {
        violations.push(violation(
            RAW_PAGE_CALLER_ORDER,
            format!(
                "paged at a limit of {PAGE_LIMIT} under the dispatched order, the concatenation \
                 of the pages was {delivered_ids:?}; the fixture's own `(tenant_id, window_end)` \
                 order (independent of any plugin call) is {expected:?}. The walk must deliver \
                 the fixture in the order the dispatched `(tenant_id, window_end, id)` key \
                 actually sorts it.",
                expected = fixtures.ascending_ids,
            ),
        ));
    }

    // Assertion 2b: paged concatenation equals the single-call read, in
    // order -- so the seek reads the same order the unpaginated `ORDER BY`
    // did, not merely an order that happens to match the fixture.
    let single_ids: Vec<Uuid> = single.items.iter().map(|record| record.id).collect();
    if delivered_ids != single_ids {
        violations.push(violation(
            RAW_PAGE_CALLER_ORDER,
            format!(
                "paged at a limit of {PAGE_LIMIT} under the dispatched order, the concatenation \
                 of the pages was {delivered_ids:?}; the same range and order read in one \
                 unpaginated call was {single_ids:?}. The two must agree in order: a \
                 continuation seeks under the order the caller dispatched, not the canonical one."
            ),
        ));
    }

    // Assertion 3: every returned keyset has one value per dispatched key,
    // in the dispatched direction.
    violations.extend(assert_keyset_shape(
        &pages,
        query.order.0.len(),
        SortDir::Asc,
    ));

    // Assertion 4: a sibling walk under the identical order **descending**
    // reverses the ascending sequence and shapes its keysets' direction as
    // `Desc`. No other dispatch in this suite sends a descending order, so
    // nothing else could fail a plugin that hardcodes `SortDir::Asc` into every
    // keyset it mints — such a plugin passes assertion 3 unconditionally.
    violations.extend(a_descending_walk_reverses_the_ascending_one(plugin, &fixtures).await);

    violations
}

/// Assertion 4: dispatches the fixture's own range under
/// `(tenant_id, window_end, id)` **descending** and asserts the walk
/// reverses [`CallerOrderFixtures::ascending_ids`] and that every returned
/// keyset carries [`SortDir::Desc`] and the dispatched arity.
///
/// Reuses `fixtures.records`, already persisted by [`raw_page_caller_order`]
/// before this is called — no second set of fixtures, since the direction
/// is a property of how the *read* is dispatched, not of what is stored.
async fn a_descending_walk_reverses_the_ascending_one(
    plugin: &dyn UsageCollectorPluginV1,
    fixtures: &CallerOrderFixtures,
) -> Vec<ContractViolation> {
    let query = caller_order_query(PAGE_LIMIT, SortDir::Desc);
    let pages = match walk_with_keysets(plugin, &fixtures.meter, fixtures.range, &query).await {
        Ok(pages) => pages,
        Err(detail) => return vec![violation(RAW_PAGE_CALLER_ORDER, detail)],
    };

    let mut violations = Vec::new();
    let delivered: Vec<StoredUsageRecord> =
        pages.iter().flat_map(|(items, _)| items.clone()).collect();
    let delivered_ids: Vec<Uuid> = delivered.iter().map(|record| record.id).collect();
    let expected: Vec<Uuid> = fixtures.ascending_ids.iter().rev().copied().collect();
    if delivered_ids != expected {
        violations.push(violation(
            RAW_PAGE_CALLER_ORDER,
            format!(
                "paged at a limit of {PAGE_LIMIT} under `(tenant_id, window_end, id)` \
                 descending, the concatenation of the pages was {delivered_ids:?}; the \
                 fixture's own ascending order reversed is {expected:?}. A descending dispatch \
                 must deliver the exact reverse of the ascending one."
            ),
        ));
    }

    violations.extend(assert_keyset_shape(
        &pages,
        query.order.0.len(),
        SortDir::Desc,
    ));
    violations
}

/// Checks every page's returned keyset (where one was returned) carries
/// exactly `order_arity` values and `expected_direction` — shared between
/// the ascending and descending walks so the two cannot silently drift
/// apart on what "the keyset's shape" means.
fn assert_keyset_shape(
    pages: &[(Vec<StoredUsageRecord>, Option<Keyset>)],
    order_arity: usize,
    expected_direction: SortDir,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for (page_number, (_, next)) in pages.iter().enumerate() {
        let Some(keyset) = next else { continue };
        if keyset.values().len() != order_arity {
            violations.push(violation(
                RAW_PAGE_CALLER_ORDER,
                format!(
                    "page {page_number}'s keyset carried {observed} value(s); the dispatched \
                     order has {order_arity} keys (`tenant_id`, `window_end`, `id`), and a \
                     keyset carries one boundary value per key of the order it was dispatched \
                     under. A plugin reading a fixed position for `window_end` or `id` rather \
                     than the order it was actually given produces exactly this shape of defect.",
                    observed = keyset.values().len(),
                ),
            ));
        }
        if keyset.direction() != expected_direction {
            violations.push(violation(
                RAW_PAGE_CALLER_ORDER,
                format!(
                    "page {page_number}'s keyset carried direction {direction:?}; the dispatched \
                     order's direction is {expected_direction:?} throughout, and a keyset's \
                     direction is that order's single direction.",
                    direction = keyset.direction(),
                ),
            ));
        }
    }
    violations
}

/// The query this check dispatches: `(tenant_id, window_end, id)` in
/// `dir`, no caller filter beyond the meter and range the SPI call carries
/// as typed parameters.
fn caller_order_query(limit: u64, dir: SortDir) -> ODataQuery {
    ODataQuery::new()
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: "tenant_id".to_owned(),
                dir,
            },
            OrderKey {
                field: "window_end".to_owned(),
                dir,
            },
            OrderKey {
                field: "id".to_owned(),
                dir,
            },
        ]))
        .with_limit(limit)
}

/// Walks a paginated range dispatched under `query`, returning each page's
/// entries alongside the keyset it returned (if any). `Err` carries a
/// ready-to-report detail.
async fn walk_with_keysets(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterRef,
    range: TimeRange,
    query: &ODataQuery,
) -> Result<Vec<(Vec<StoredUsageRecord>, Option<Keyset>)>, String> {
    let mut pages: Vec<(Vec<StoredUsageRecord>, Option<Keyset>)> = Vec::new();
    let mut keyset: Option<Keyset> = None;
    for page_number in 1..=MAX_PAGES {
        let page = plugin
            .list_usage_records(meter, range, query, &[], keyset.as_ref())
            .await
            .map_err(|err| {
                format!(
                    "`list_usage_records` failed on page {page_number} of a caller-order walk: \
                     {err}"
                )
            })?;
        let next = page.next.clone();
        pages.push((page.items, next.clone()));
        match next {
            Some(ks) => keyset = Some(ks),
            None => return Ok(pages),
        }
    }
    Err(format!(
        "a caller-order walk followed its keyset for {MAX_PAGES} pages without reaching a page \
         reporting no continuation. A plugin whose keyset does not advance would otherwise loop \
         forever; the cap reports it as a violation naming the plugin instead of hanging the \
         suite."
    ))
}

/// This check's four fixture entries and the range and meter selecting
/// them.
struct CallerOrderFixtures {
    records: Vec<StoredUsageRecord>,
    /// The ids in the order `(tenant_id, window_end, id)` ascending must
    /// deliver them, derived from [`caller_order_fixtures`]'s own layout
    /// rather than from any plugin call — see that function's doc for the
    /// derivation. This is the independent oracle the check compares a
    /// plugin's delivered order against; comparing two plugin calls against
    /// each other instead would let a plugin applying one consistent wrong
    /// order to both pass.
    ascending_ids: Vec<Uuid>,
    meter: MeterRef,
    range: TimeRange,
}

/// Builds entries across two tenants and interleaved covered periods, chosen so
/// that sorting by `(tenant_id, window_end, id)` gives a *different* sequence
/// from the canonical `(window_end, id)` order — which is the whole point: a
/// plugin that silently falls back to the canonical order could still pass a
/// weaker check that never built a case where the two orders disagree.
///
/// [`contract_tenant`] mints the two tenants from the block clear of every named
/// id, at indices chosen so the lower-indexed tenant's canonical `Uuid`
/// rendering sorts before the higher-indexed one's: the two differ only in the
/// low byte of the block, so index order and string order agree.
fn caller_order_fixtures() -> Result<CallerOrderFixtures, String> {
    let window_from = check_window_from(RAW_PAGE_CALLER_ORDER, "main");
    let tenant_low = contract_tenant(90);
    let tenant_high = contract_tenant(91);
    if tenant_low.to_string() >= tenant_high.to_string() {
        return Err(format!(
            "the check's own two tenants ({tenant_low}, {tenant_high}) do not render in the \
             order this check assumes, so the order-honouring assertion would prove nothing"
        ));
    }

    // (tenant index, hour offset): the entry's covered period is
    // [window_from + hour, window_from + hour + 1).
    //
    // Canonical `(window_end, id)` order groups by hour: hour 1's two
    // entries (tie-broken by id) first, then hour 2, then hour 3.
    // `(tenant_id, window_end, id)` groups by tenant first: both of
    // `tenant_low`'s entries (by window_end: hour 1 then hour 2), then
    // both of `tenant_high`'s (hour 1 then hour 3). The two sequences
    // disagree on every position but the first.
    let layout = [
        (tenant_low, 2_i64),
        (tenant_low, 1),
        (tenant_high, 1),
        (tenant_high, 3),
    ];

    let meter = contract_meter()?;
    let mut records = Vec::with_capacity(layout.len());
    for (index, (tenant, hour)) in layout.into_iter().enumerate() {
        let window_start = window_from.saturating_add(time::Duration::hours(hour));
        let window_end = window_from.saturating_add(time::Duration::hours(hour + 1));
        let idempotency_key = IdempotencyKey::new(format!("{RAW_PAGE_CALLER_ORDER}-{index}"))
            .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
        let quantity =
            UsageQuantity::parse("1").map_err(|err| format!("`1` is a valid quantity: {err}"))?;
        records.push(fixture_record_on(
            &meter,
            tenant,
            &idempotency_key,
            quantity,
            CONTRACT_ACCEPTED_AT,
            window_start,
            window_end,
        )?);
    }

    // The independent expectation: sort the layout's own indices by
    // `(tenant, hour)` -- the same key `(tenant_id, window_end)` the
    // dispatched order leads with, `id` never entering since no two
    // entries here share a `(tenant, hour)` pair -- and read off the ids in
    // that order. Built from the layout table above, never from a call to
    // the plugin under test.
    let mut ascending_order: Vec<usize> = (0..layout.len()).collect();
    ascending_order.sort_by_key(|&index| layout[index]);
    let ascending_ids: Vec<Uuid> = ascending_order
        .into_iter()
        .map(|index| records[index].id)
        .collect();

    // Exclusive upper bound on `window_end`: the latest entry's `window_end`
    // is `window_from + 4` hours (hour 3's record), so the range has to
    // reach one hour past that to select it.
    let window_to = window_from.saturating_add(time::Duration::hours(5));
    let range = TimeRange::new(window_from, window_to)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    Ok(CallerOrderFixtures {
        records,
        ascending_ids,
        meter,
        range,
    })
}
