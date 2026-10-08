//! The `raw-page-keyset-walk` check.
//!
//! See [`raw_page_keyset_walk`] for what it asserts. The module holds that
//! check's fixtures, a shared paginated-walk helper, and nothing else.

use std::collections::BTreeSet;

use toolkit_odata::ODataQuery;
use uuid::Uuid;

use crate::contract::fixtures::{
    check_meter, check_window_from, contract_query, fixture_record_for_tenant, fixture_record_on,
    seed_usage_record, violation,
};
use crate::contract::{ContractViolation, HARNESS_FAULT, RAW_PAGE_KEYSET_WALK};
use crate::keyset::Keyset;
use crate::models::IdempotencyKey;
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::stored::{MeterRef, StoredUsageRecord};
use crate::time_range::TimeRange;

/// A plugin whose keyset does not advance would loop forever. The cap is
/// the fixture count plus a margin, and reaching it is a violation in its
/// own right rather than a harness timeout, so the failure names the
/// plugin.
const MAX_PAGES: usize = 32;

/// `raw-page-keyset-walk` — a paginated walk of a range returns every entry
/// exactly once, seeks rather than skips, and trims its look-ahead.
///
/// Each of these is a separate [`violation`] so a failing run names which:
///
/// 1. Walking a range page by page returns every entry in it exactly once,
///    compared against the same range read in one unpaginated call. This
///    fixture's **selected** sequence is not contiguous with its **stored** one
///    — one entry in the middle of the covered window is persisted under a meter
///    the dispatched read does not name. See
///    [`a_walk_over_a_range_with_a_hole_delivers_every_selected_entry_once`] for
///    what the hole buys and what it does not.
/// 2. A page boundary where several entries share one `window_end` loses and
///    repeats nothing — the case the `(window_end, id)` tiebreaker exists for,
///    and the one a plugin ordering on `window_end` alone would fail.
/// 3. A first page carries no seek: dispatched with `keyset: None`, the page's
///    first entry is the first of the selected range in the effective order.
/// 4. A continuation begins strictly after the boundary row: the boundary row
///    does not reappear as the first entry of the page that follows it.
/// 5. The look-ahead is trimmed: a page filling exactly to the limit with
///    nothing further reports `next: None`, and a page with more reports `Some`.
///    Both arms are asserted, because a plugin hardcoding `None` passes the
///    first and one hardcoding `Some` passes the second.
pub async fn raw_page_keyset_walk(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let mut violations =
        a_walk_over_a_range_with_a_hole_delivers_every_selected_entry_once(plugin).await;
    violations.extend(a_tied_boundary_loses_and_repeats_nothing(plugin).await);
    violations.extend(the_look_ahead_is_trimmed(plugin).await);
    violations
}

/// Walks a paginated range by threading each page's `next` into the
/// following call's `keyset`, under the canonical `(window_end, id)`
/// order. Returns the pages in order; `Err` carries a ready-to-report
/// detail.
async fn walk_raw_pages(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterRef,
    range: TimeRange,
    query: &ODataQuery,
) -> Result<Vec<Vec<StoredUsageRecord>>, String> {
    let mut pages: Vec<Vec<StoredUsageRecord>> = Vec::new();
    let mut keyset: Option<Keyset> = None;
    for page_number in 1..=MAX_PAGES {
        let page = plugin
            .list_usage_records(meter, range, query, &[], keyset.as_ref())
            .await
            .map_err(|err| {
                format!(
                    "`list_usage_records` failed on page {page_number} of a raw-page walk: {err}"
                )
            })?;
        let next = page.next.clone();
        pages.push(page.items);
        match next {
            Some(ks) => keyset = Some(ks),
            None => return Ok(pages),
        }
    }
    Err(format!(
        "a raw-page walk followed its keyset for {MAX_PAGES} pages without reaching a page \
         reporting no continuation. A plugin whose keyset does not advance would otherwise loop \
         forever; the cap reports it as a violation naming the plugin instead of hanging the \
         suite."
    ))
}

/// Whether `expected` and `delivered` carry the same ids, each exactly
/// once, naming the difference when they do not.
fn assert_exactly_once(expected: &[Uuid], delivered: &[Uuid]) -> Option<String> {
    let expected_set: BTreeSet<Uuid> = expected.iter().copied().collect();
    let delivered_set: BTreeSet<Uuid> = delivered.iter().copied().collect();
    if delivered.len() == expected.len() && delivered_set == expected_set {
        return None;
    }
    let missing: Vec<Uuid> = expected_set.difference(&delivered_set).copied().collect();
    let repeated: Vec<Uuid> = {
        let mut seen = BTreeSet::new();
        delivered
            .iter()
            .copied()
            .filter(|id| !seen.insert(*id))
            .collect()
    };
    let unexpected: Vec<Uuid> = delivered_set.difference(&expected_set).copied().collect();
    Some(format!(
        "the walk delivered {delivered:?} against an expected {expected:?}. Missing: \
         {missing:?}. Repeated: {repeated:?}. Unexpected: {unexpected:?}."
    ))
}

// Requirements 1, 3 and 4: the full walk over a range with a hole

/// How many one-hour fixture slots [`hole_fixtures`] lays down, including
/// the one excluded by meter.
const HOLE_SLOT_COUNT: i64 = 6;

/// Which of [`HOLE_SLOT_COUNT`]'s slots is persisted under a meter this
/// check's own read does not name — the hole.
const HOLE_EXCLUDED_SLOT: i64 = 2;

/// The page limit the walk dispatches: small enough that the five selected
/// entries span three pages (2, 2, 1), which is what gives the walk the
/// two continuations [`a_walk_over_a_range_with_a_hole_delivers_every_selected_entry_once`]'s
/// doc explains the need for.
const HOLE_WALK_PAGE_LIMIT: u64 = 2;

/// The six persisted entries and the meter the read selects on.
struct HoleFixtures {
    /// All six entries, in slot order, each paired with the meter it is
    /// written under. [`HOLE_EXCLUDED_SLOT`]'s entry is among them,
    /// persisted under a meter of its own and so outside the selection —
    /// which is why the meter travels with the entry rather than being one
    /// value for the whole vector.
    persisted: Vec<(MeterRef, StoredUsageRecord)>,
    /// The ids the read is expected to deliver: every entry but the
    /// excluded one's.
    selected_ids: Vec<Uuid>,
    /// The meter the read dispatches against.
    meter: MeterRef,
    /// The range covering every slot, selected and excluded alike.
    range: TimeRange,
}

/// Builds [`HOLE_SLOT_COUNT`] one-hour entries, with [`HOLE_EXCLUDED_SLOT`]'s
/// persisted under a meter of its own so the covered-period range the check
/// dispatches selects the rest but not it.
///
/// A single contiguous [`TimeRange`] cannot itself skip a middle `window_end`
/// while admitting the slots on both sides of it — selection on
/// `from <= window_end < to` is necessarily contiguous in `window_end` — so the
/// hole is cut on the meter dimension instead, which keeps that entry's
/// **`window_end` in the middle of the dispatched range** while the read's own
/// meter filter leaves it out.
fn hole_fixtures() -> Result<HoleFixtures, String> {
    let window_from = check_window_from(RAW_PAGE_KEYSET_WALK, "main");
    let excluded_meter = check_meter(RAW_PAGE_KEYSET_WALK, "hole")?;
    let mut persisted = Vec::with_capacity(usize::try_from(HOLE_SLOT_COUNT).unwrap_or(0));
    let mut selected_ids = Vec::new();
    for slot in 0..HOLE_SLOT_COUNT {
        let window_start = window_from.saturating_add(time::Duration::hours(slot));
        let window_end = window_from.saturating_add(time::Duration::hours(slot + 1));
        let idempotency_key = IdempotencyKey::new(format!("{RAW_PAGE_KEYSET_WALK}-hole-{slot}"))
            .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
        let quantity =
            UsageQuantity::parse("1").map_err(|err| format!("`1` is a valid quantity: {err}"))?;
        let shared_meter = crate::contract::fixtures::contract_meter()?;
        let record = if slot == HOLE_EXCLUDED_SLOT {
            let record = fixture_record_on(
                &excluded_meter,
                crate::contract::fixtures::CONTRACT_TENANT_ID,
                &idempotency_key,
                quantity,
                crate::contract::fixtures::CONTRACT_ACCEPTED_AT,
                window_start,
                window_end,
            )?;
            (excluded_meter.clone(), record)
        } else {
            let record = fixture_record_for_tenant(
                crate::contract::fixtures::CONTRACT_TENANT_ID,
                &idempotency_key,
                quantity,
                window_start,
                window_end,
            )?;
            selected_ids.push(record.id);
            (shared_meter, record)
        };
        persisted.push(record);
    }
    // The upper bound is exclusive on `window_end`
    // (`TimeRange::contains_window_end`), and the last slot's `window_end`
    // is exactly `window_from + HOLE_SLOT_COUNT` hours, so the range has to
    // reach one hour past that to select it.
    let window_to = window_from.saturating_add(time::Duration::hours(HOLE_SLOT_COUNT + 1));
    let range = TimeRange::new(window_from, window_to)
        .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let meter = crate::contract::fixtures::contract_meter()?;
    Ok(HoleFixtures {
        persisted,
        selected_ids,
        meter,
        range,
    })
}

/// Requirements 1, 3 and 4.
///
/// # What actually separates a seek from a fixed-size skip here
///
/// A plugin that resumes by skipping a **fixed count** of rows — the dispatched
/// order's arity, say, rather than the keyset's actual boundary values — agrees
/// with a correct seek on a single continuation exactly when the page limit
/// happens to equal that count. It disagrees the moment either of two
/// independent things holds:
///
/// * the walk needs a **second** continuation, because the fixed count never
///   accumulates across calls — it reproduces an earlier page's view instead of
///   advancing past it; or
/// * the page limit does not **equal** the fixed count, which breaks the very
///   first continuation.
///
/// This fixture is built on the first: [`HOLE_WALK_PAGE_LIMIT`] equals the
/// canonical order's arity, but the selected entries span three pages, so the
/// third page's resumption is a second continuation.
/// [`raw_page_caller_order`](super::raw_page_caller_order::raw_page_caller_order)
/// is built on the second instead, which catches the identical mistake on the
/// very first continuation and is the more robust of the two.
///
/// **The hole is not what does the separating**, measured rather than assumed:
/// the identical fixture with the hole closed still needs a second continuation
/// at this page limit and still catches the mistake. What the hole buys is a
/// selected-sequence-not-contiguous-with-stored-sequence scenario worth having
/// on its own merits.
async fn a_walk_over_a_range_with_a_hole_delivers_every_selected_entry_once(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match hole_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{RAW_PAGE_KEYSET_WALK}` hole \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in the \
                     plugin under test: {detail}"
                ),
            )];
        }
    };

    for (meter, record) in &fixtures.persisted {
        if let Err(err) = seed_usage_record(plugin, meter, record.clone()).await {
            return vec![violation(
                RAW_PAGE_KEYSET_WALK,
                format!(
                    "`create_usage_records` refused a hole-walk fixture (record {id}): {err}",
                    id = record.id,
                ),
            )];
        }
    }

    let walk_query = contract_query(HOLE_WALK_PAGE_LIMIT);
    let pages = match walk_raw_pages(plugin, &fixtures.meter, fixtures.range, &walk_query).await {
        Ok(pages) => pages,
        Err(detail) => return vec![violation(RAW_PAGE_KEYSET_WALK, detail)],
    };

    let single_call_query =
        contract_query(u64::try_from(fixtures.selected_ids.len() + 1).unwrap_or(u64::MAX));
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
                RAW_PAGE_KEYSET_WALK,
                format!(
                    "`list_usage_records` failed reading the hole-walk's range in one unpaginated \
                     call, so the paginated walk had nothing to be compared against: {err}"
                ),
            )];
        }
    };

    let mut violations = Vec::new();

    // Requirement 1: every selected entry exactly once, and in the fixture's own
    // ascending `window_end` order — an expectation independent of any plugin
    // call. Comparing only against the single-call read below would let a plugin
    // that applies one consistent wrong order to both reads pass.
    let delivered: Vec<StoredUsageRecord> = pages.iter().flatten().cloned().collect();
    let delivered_ids: Vec<Uuid> = delivered.iter().map(|record| record.id).collect();
    let single_ids: Vec<Uuid> = single.items.iter().map(|record| record.id).collect();
    if let Some(detail) = assert_exactly_once(&fixtures.selected_ids, &delivered_ids) {
        violations.push(violation(
            RAW_PAGE_KEYSET_WALK,
            format!(
                "walking the hole-containing range page by page did not deliver every selected \
                 entry exactly once: {detail}"
            ),
        ));
    }
    if delivered_ids != fixtures.selected_ids {
        violations.push(violation(
            RAW_PAGE_KEYSET_WALK,
            format!(
                "walking the hole-containing range page by page delivered {delivered_ids:?}; the \
                 fixture's own ascending `window_end` order is {expected:?} (`hole_fixtures` \
                 builds `selected_ids` in that order, independent of any plugin call). The walk \
                 must deliver the selection in ascending `window_end` order.",
                expected = fixtures.selected_ids,
            ),
        ));
    }
    if delivered_ids != single_ids {
        violations.push(violation(
            RAW_PAGE_KEYSET_WALK,
            format!(
                "the paginated walk delivered {delivered_ids:?} and the same range read in one \
                 unpaginated call delivered {single_ids:?}. The two must agree, in order: a \
                 continuation is a seek under the same order the single call used, not a second \
                 way of reading the range."
            ),
        ));
    }

    // Requirement 3: the first page carries no seek. Compared against the
    // fixture's own first selected id rather than against the single-call read,
    // which is itself the plugin's own second opinion rather than an independent
    // expectation.
    if let Some(first_page) = pages.first() {
        let expected_first = fixtures.selected_ids.first().copied();
        match (first_page.first().map(|record| record.id), expected_first) {
            (Some(observed), Some(expected)) if observed == expected => {}
            (Some(observed), Some(expected)) => violations.push(violation(
                RAW_PAGE_KEYSET_WALK,
                format!(
                    "dispatched with `keyset: None`, the first page's first entry was {observed}; \
                     the first entry of the selected range in ascending `window_end` order is \
                     {expected} (the fixture's own first selected slot). A first page carries no \
                     seek predicate."
                ),
            )),
            (None, Some(_)) => violations.push(violation(
                RAW_PAGE_KEYSET_WALK,
                "dispatched with `keyset: None`, the first page carried no entries at all, \
                 though the range selects some."
                    .to_owned(),
            )),
            (_, None) => {}
        }
    }

    // Requirement 4: a continuation begins strictly after the boundary row.
    for (page_number, pair) in pages.windows(2).enumerate() {
        let (previous, next) = (&pair[0], &pair[1]);
        if let (Some(boundary), Some(first_of_next)) = (previous.last(), next.first())
            && boundary.id == first_of_next.id
        {
            violations.push(violation(
                RAW_PAGE_KEYSET_WALK,
                format!(
                    "page {page_number} ended on record {id}, and the following page's first \
                     entry was that same record. A continuation must begin strictly after the \
                     boundary row, not at it.",
                    id = boundary.id,
                ),
            ));
        }
    }

    violations
}

// Requirement 2: a tied boundary

/// How many entries [`tie_fixtures`] stacks on one `window_end`. Three, so
/// a page limit of two puts the boundary inside the tie.
const TIE_ENTRY_COUNT: usize = 3;

/// The page limit dispatched over the tied fixtures: inside the tie group.
const TIE_PAGE_LIMIT: u64 = 2;

/// [`TIE_ENTRY_COUNT`] entries sharing one `window_end`, and the range selecting
/// them.
struct TieFixtures {
    records: Vec<StoredUsageRecord>,
    meter: MeterRef,
    range: TimeRange,
}

/// Builds [`TIE_ENTRY_COUNT`] entries on one covered period, each under its
/// own idempotency key so they derive distinct ids — the case the
/// `(window_end, id)` tiebreaker exists for.
fn tie_fixtures() -> Result<TieFixtures, String> {
    let window_from = check_window_from(RAW_PAGE_KEYSET_WALK, "ties");
    let window_end = window_from.saturating_add(time::Duration::hours(1));
    let mut records = Vec::with_capacity(TIE_ENTRY_COUNT);
    for index in 0..TIE_ENTRY_COUNT {
        let idempotency_key = IdempotencyKey::new(format!("{RAW_PAGE_KEYSET_WALK}-tie-{index}"))
            .map_err(|err| format!("the check's own idempotency key is invalid: {err}"))?;
        let quantity =
            UsageQuantity::parse("1").map_err(|err| format!("`1` is a valid quantity: {err}"))?;
        records.push(fixture_record_for_tenant(
            crate::contract::fixtures::CONTRACT_TENANT_ID,
            &idempotency_key,
            quantity,
            window_from,
            window_end,
        )?);
    }
    let range = TimeRange::new(
        window_from,
        window_end.saturating_add(time::Duration::hours(1)),
    )
    .map_err(|err| format!("the check could not build its own read range: {err}"))?;
    let meter = crate::contract::fixtures::contract_meter()?;
    Ok(TieFixtures {
        records,
        meter,
        range,
    })
}

/// Requirement 2.
async fn a_tied_boundary_loses_and_repeats_nothing(
    plugin: &dyn UsageCollectorPluginV1,
) -> Vec<ContractViolation> {
    let fixtures = match tie_fixtures() {
        Ok(fixtures) => fixtures,
        Err(detail) => {
            return vec![violation(
                HARNESS_FAULT,
                format!(
                    "the contract suite could not build its own `{RAW_PAGE_KEYSET_WALK}` tie \
                     fixtures, so nothing was submitted. This is a fault in the suite, not in \
                     the plugin under test: {detail}"
                ),
            )];
        }
    };

    for record in &fixtures.records {
        if let Err(err) = seed_usage_record(plugin, &fixtures.meter, record.clone()).await {
            return vec![violation(
                RAW_PAGE_KEYSET_WALK,
                format!(
                    "`create_usage_records` refused a tied-boundary fixture (record {id}): {err}",
                    id = record.id,
                ),
            )];
        }
    }

    let query = contract_query(TIE_PAGE_LIMIT);
    let pages = match walk_raw_pages(plugin, &fixtures.meter, fixtures.range, &query).await {
        Ok(pages) => pages,
        Err(detail) => return vec![violation(RAW_PAGE_KEYSET_WALK, detail)],
    };

    let expected: Vec<Uuid> = fixtures.records.iter().map(|record| record.id).collect();
    let delivered: Vec<Uuid> = pages.iter().flatten().map(|record| record.id).collect();
    match assert_exactly_once(&expected, &delivered) {
        Some(detail) => vec![violation(
            RAW_PAGE_KEYSET_WALK,
            format!(
                "a page boundary falling inside three entries sharing one `window_end` must lose \
                 and repeat nothing: {detail}"
            ),
        )],
        None => Vec::new(),
    }
}

// Requirement 5: the look-ahead is trimmed

/// The page limit both look-ahead sub-cases dispatch.
const LOOK_AHEAD_PAGE_LIMIT: u64 = 2;

/// Requirement 5, both arms.
async fn the_look_ahead_is_trimmed(plugin: &dyn UsageCollectorPluginV1) -> Vec<ContractViolation> {
    let window_from = check_window_from(RAW_PAGE_KEYSET_WALK, "look_ahead");
    let meter = match crate::contract::fixtures::contract_meter() {
        Ok(meter) => meter,
        Err(err) => {
            return vec![violation(
                HARNESS_FAULT,
                format!("the check's own meter is invalid: {err}"),
            )];
        }
    };

    let mut violations = Vec::new();
    violations.extend(
        look_ahead_case(plugin, &meter, window_from, 0, 2, false)
            .await
            .unwrap_or_else(|v| v),
    );
    violations.extend(
        look_ahead_case(plugin, &meter, window_from, 10, 3, true)
            .await
            .unwrap_or_else(|v| v),
    );
    violations
}

/// One look-ahead sub-case: persists `entry_count` one-hour entries starting
/// `hour_offset` hours past `window_from`, reads them at
/// [`LOOK_AHEAD_PAGE_LIMIT`], and asserts whether `next` is `Some` according to
/// `expect_more`.
///
/// Both arms report through the same `Vec`; the `Result` only threads the
/// early-return shape through `?`.
async fn look_ahead_case(
    plugin: &dyn UsageCollectorPluginV1,
    meter: &MeterRef,
    window_from: time::OffsetDateTime,
    hour_offset: i64,
    entry_count: i64,
    expect_more: bool,
) -> Result<Vec<ContractViolation>, Vec<ContractViolation>> {
    let case_from = window_from.saturating_add(time::Duration::hours(hour_offset));
    // Exclusive upper bound again: the last entry's `window_end` is
    // `case_from + entry_count` hours.
    let case_to = case_from.saturating_add(time::Duration::hours(entry_count + 1));
    let range = TimeRange::new(case_from, case_to).map_err(|err| {
        vec![violation(
            HARNESS_FAULT,
            format!("the check could not build its own look-ahead range: {err}"),
        )]
    })?;

    for slot in 0..entry_count {
        let window_start = case_from.saturating_add(time::Duration::hours(slot));
        let window_end = case_from.saturating_add(time::Duration::hours(slot + 1));
        let idempotency_key = IdempotencyKey::new(format!(
            "{RAW_PAGE_KEYSET_WALK}-look-ahead-{hour_offset}-{slot}"
        ))
        .map_err(|err| {
            vec![violation(
                HARNESS_FAULT,
                format!("the check's own idempotency key is invalid: {err}"),
            )]
        })?;
        let quantity = UsageQuantity::parse("1").map_err(|err| {
            vec![violation(
                HARNESS_FAULT,
                format!("`1` is a valid quantity: {err}"),
            )]
        })?;
        let record = fixture_record_for_tenant(
            crate::contract::fixtures::CONTRACT_TENANT_ID,
            &idempotency_key,
            quantity,
            window_start,
            window_end,
        )
        .map_err(|detail| vec![violation(HARNESS_FAULT, detail)])?;
        if let Err(err) = seed_usage_record(plugin, meter, record.clone()).await {
            return Ok(vec![violation(
                RAW_PAGE_KEYSET_WALK,
                format!(
                    "`create_usage_records` refused a look-ahead fixture (record {id}): {err}",
                    id = record.id,
                ),
            )]);
        }
    }

    let query = contract_query(LOOK_AHEAD_PAGE_LIMIT);
    let page = match plugin
        .list_usage_records(meter, range, &query, &[], None)
        .await
    {
        Ok(page) => page,
        Err(err) => {
            return Ok(vec![violation(
                RAW_PAGE_KEYSET_WALK,
                format!("`list_usage_records` failed over the look-ahead range: {err}"),
            )]);
        }
    };

    let has_next = page.next.is_some();
    if has_next == expect_more {
        return Ok(Vec::new());
    }
    Ok(vec![violation(
        RAW_PAGE_KEYSET_WALK,
        if expect_more {
            format!(
                "a page filling exactly to the limit of {LOOK_AHEAD_PAGE_LIMIT} with \
                 {entry_count} entries selected must report `next: Some`; it reported `None`. A \
                 plugin that hardcodes an absent continuation passes the case with nothing left \
                 and fails this one."
            )
        } else {
            format!(
                "a page with nothing past the limit of {LOOK_AHEAD_PAGE_LIMIT} must report \
                 `next: None`; it reported `Some`. A plugin that hardcodes a continuation passes \
                 the case with more left and fails this one."
            )
        },
    )])
}
