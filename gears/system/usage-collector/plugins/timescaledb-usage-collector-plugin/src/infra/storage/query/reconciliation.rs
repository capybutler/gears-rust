//! Reconciliation summary SQL: the two `FILTER`-qualified `SELECT`
//! expressions that let `accepted_count` (S1) and the fold-appropriate
//! summary (S2) read over one ranged scan (spec §9.3), instead of two.
//!
//! **This module composes [`super::aggregate`]'s fragments; it does not
//! re-derive them.** `accepted_count` itself is a bare `COUNT(*)` with no
//! predicate of its own, so it needs no builder here — the caller writes it
//! directly beside whichever of these two shapes the declared fold selects.
//! Both shapes attach [`super::aggregate::withdrawal_exclusion_clause`] as a
//! `FILTER (WHERE …)` on the aggregate call itself, which is what lets the
//! summary exclude a withdrawn pair without a second `WHERE`-scoped scan:
//!
//! - [`accrued_summary_select_expr`] — the `SUM` branch
//!   (`QuantitySummary::Accrued`): [`super::aggregate::fold_select_expr`]`(Sum)`
//!   with the exclusion spliced in as a `FILTER` clause on the `SUM(…)` call
//!   the `COALESCE` wraps.
//! - [`observation_count_select_expr`] and [`observation_latest_select_expr`]
//!   — the fixed pair every other fold reports
//!   (`QuantitySummary::Observations`): a `FILTER`-qualified `COUNT(*)`, and
//!   [`super::aggregate::LATEST_SELECT_EXPR`] with the same exclusion
//!   `FILTER`-attached to its `ARRAY_AGG(…)` call.
//!
//! `FILTER` cannot wrap a function call from the outside — it attaches
//! directly to the aggregate the clause qualifies, before any cast or
//! `COALESCE` around it — so neither shape is a plain `format!` around the
//! whole fragment. Each locates the exact substring the aggregate call ends
//! at and splices `FILTER (WHERE …)` in immediately after it. The predicate
//! itself is never re-derived: both call
//! [`super::aggregate::withdrawal_exclusion_clause`], and a `debug_assert!`
//! on the splice point plus `reconciliation_tests.rs`'s pinned expected
//! strings are what catch the two fragments drifting apart if
//! [`super::aggregate`]'s constants are ever reworded.
//!
//! What this module does **not** hold: the `WHERE` these expressions sit
//! inside (the scope, tenant and range predicates), the watermark read
//! (spec §9.3's S3, unbounded by the range), and the decode of the rows
//! they produce. All three are statement assembly, which belongs beside the
//! connection that runs it — [`super::super::record_store`], the same split
//! [`super::aggregate`] already draws with `build_aggregate_sql`.

use super::aggregate::{LATEST_SELECT_EXPR, fold_select_expr, withdrawal_exclusion_clause};
use usage_collector_sdk::AggregationFold;

/// The exact substring [`accrued_summary_select_expr`] splices a `FILTER`
/// clause after. [`fold_select_expr`]`(Sum)` is
/// `"COALESCE(SUM(r.quantity), 0)::numeric"`; this is the aggregate call
/// `FILTER` must attach to before the `COALESCE`'s closing arguments.
const SUM_CALL: &str = "SUM(r.quantity)";

/// The exact substring [`observation_latest_select_expr`] splices a `FILTER`
/// clause after: the single close-paren that ends `ARRAY_AGG(…)`'s argument
/// list — the first `)` following the tie-break order's last key — rather
/// than the second one, which closes the `(…)[1]` element pick around it.
const ARRAY_AGG_CLOSE: &str = "r.id DESC)";

/// The `SUM` branch of the reconciliation summary
/// (`QuantitySummary::Accrued`): [`fold_select_expr`]`(`[`AggregationFold::Sum`]`)`
/// with [`withdrawal_exclusion_clause`] attached as a `FILTER` clause on the
/// `SUM(r.quantity)` call the `COALESCE` wraps.
///
/// `COALESCE(SUM(r.quantity) FILTER (WHERE <exclusion>), 0)::numeric` — the
/// same defined empty-selection zero `fold_select_expr(Sum)`'s own doc
/// argues for, reached here whenever every row the ranged scan selects
/// fails the `FILTER` predicate (a range holding nothing but a withdrawn
/// pair) as much as when the scan itself selects nothing.
///
/// # Panics
///
/// Never in release. `debug_assert!`s that [`SUM_CALL`] still occurs in
/// `fold_select_expr(Sum)`'s output, so a reworded aggregate.rs constant
/// fails a debug/test build here rather than silently splicing `FILTER`
/// into the wrong place.
// @cpt-dod:cpt-cf-uc-plugin-dod-summary-excludes-withdrawn-pairs:p2
#[must_use]
pub fn accrued_summary_select_expr() -> String {
    let unfiltered = fold_select_expr(AggregationFold::Sum);
    debug_assert!(
        unfiltered.contains(SUM_CALL),
        "fold_select_expr(Sum) no longer contains {SUM_CALL:?}; \
         accrued_summary_select_expr's splice point has drifted: {unfiltered}"
    );
    unfiltered.replacen(
        SUM_CALL,
        &format!(
            "{SUM_CALL} FILTER (WHERE {})",
            withdrawal_exclusion_clause()
        ),
        1,
    )
}

/// Half of the `Observations` branch's fixed pair
/// (`QuantitySummary::Observations::count`): a `FILTER`-qualified
/// `COUNT(*)`, excluding a withdrawn pair the same way
/// [`accrued_summary_select_expr`] does for the accrued branch.
///
/// `COUNT(*) FILTER (WHERE <exclusion>)` — never `NULL`, `0` over a
/// selection every row of which fails the filter, exactly as an unqualified
/// `COUNT(*)` is `0` over zero rows.
#[must_use]
pub fn observation_count_select_expr() -> String {
    format!("COUNT(*) FILTER (WHERE {})", withdrawal_exclusion_clause())
}

/// The other half of the `Observations` branch's fixed pair
/// (`QuantitySummary::Observations::latest`):
/// [`LATEST_SELECT_EXPR`] with [`withdrawal_exclusion_clause`] attached as a
/// `FILTER` clause on the `ARRAY_AGG(…)` call it picks the head of.
///
/// `(ARRAY_AGG(r.quantity ORDER BY …) FILTER (WHERE <exclusion>))[1]::numeric`
/// — `NULL` when every row the ranged scan selects fails the `FILTER`
/// predicate, read back as the `latest` field's defined absence over an
/// empty selection.
///
/// # Panics
///
/// Never in release. `debug_assert!`s that [`ARRAY_AGG_CLOSE`] still occurs
/// in [`LATEST_SELECT_EXPR`], for the same reason
/// [`accrued_summary_select_expr`]'s assertion exists.
#[must_use]
pub fn observation_latest_select_expr() -> String {
    debug_assert!(
        LATEST_SELECT_EXPR.contains(ARRAY_AGG_CLOSE),
        "LATEST_SELECT_EXPR no longer contains {ARRAY_AGG_CLOSE:?}; \
         observation_latest_select_expr's splice point has drifted: {LATEST_SELECT_EXPR}"
    );
    LATEST_SELECT_EXPR.replacen(
        ARRAY_AGG_CLOSE,
        &format!(
            "{ARRAY_AGG_CLOSE} FILTER (WHERE {})",
            withdrawal_exclusion_clause()
        ),
        1,
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "reconciliation_tests.rs"]
mod reconciliation_tests;
