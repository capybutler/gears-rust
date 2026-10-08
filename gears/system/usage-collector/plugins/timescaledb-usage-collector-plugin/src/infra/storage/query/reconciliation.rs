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
//! **The two statements these compose into live here as well**, beside the
//! fragments: [`build_reconciliation_summary_sql`] assembles S1∧S2 over one
//! ranged scan, and [`build_reconciliation_watermarks_sql`] assembles S3 —
//! unbounded by the range, and a separate statement for the reason its own doc
//! gives. That is the shape [`super::feed`], [`super::rollup`] and
//! [`super::aggregate`] all take: one module owns a path's statement end to
//! end, so which fragment lands in which clause is readable without holding a
//! second file open.
//!
//! What this module does **not** hold: anything needing a connection or a
//! `sqlx` type. Both builders are pure — a statement and its binds in
//! placeholder order — and the round trip that runs them, together with the
//! decode of the rows they produce, is
//! [`super::super::record_store`]'s `reconciliation`.

use toolkit_odata::ast;
use uuid::Uuid;

use usage_collector_sdk::{AggregationFold, QuantitySummary, TimeRange};

use super::aggregate::{LATEST_SELECT_EXPR, fold_select_expr, withdrawal_exclusion_clause};
use super::bind::SqlBind;
use super::translate::{SqlCtx, translate_scope};
use super::{ledger_from_clause, push_meter_and_range_clauses, push_meter_clause};

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

/// One assembled S1∧S2 statement (spec §9.3): `accepted_count` and the
/// fold-appropriate summary, both read off one ranged scan of the scope.
#[derive(Debug)]
pub struct ReconciliationSummaryStatement {
    pub sql: String,
    pub binds: Vec<SqlBind>,
}

/// Build spec §9.3's S1 (`accepted_count`) and S2 (the summary), composed
/// as one statement over one ranged scan.
///
/// ```sql
/// SELECT COUNT(*), <accrued_summary_select_expr()>
/// FROM usage_records r
/// WHERE r.gts_type_uuid = $1 AND r.type_key = (…) AND r.window_end >= $2
///   AND r.window_end < $3 AND r.tenant_id = $4 AND <translated scope>
/// ```
/// or, for every fold but `Sum`:
/// ```sql
/// SELECT COUNT(*), <observation_count_select_expr()>, <observation_latest_select_expr()>
/// FROM usage_records r
/// WHERE … (the same five predicates)
/// ```
///
/// **`accepted_count` carries no predicate of its own** — it is the bare
/// `COUNT(*)` the DESIGN gives it, over every row the `WHERE` selects,
/// invalidations and withdrawn pairs included. The summary excludes a
/// withdrawn pair through the `FILTER` clause each of
/// [`accrued_summary_select_expr`]/[`observation_count_select_expr`]/
/// [`observation_latest_select_expr`] already attaches, rather than a second
/// `WHERE`-scoped scan: the two figures differ by exactly that one predicate,
/// so `FILTER` lets `PostgreSQL` answer both from the rows it reads once.
///
/// `FROM` is [`ledger_from_clause`], shared with every other read path; the
/// meter and the range are [`push_meter_and_range_clauses`], shared with
/// [`super::list`] and [`super::aggregate`]; the tenant and the compiled scope
/// are this statement's own, bound after the range in that order.
///
/// **This statement carries no `GROUP BY` and `PostgreSQL` still answers exactly
/// one row for it**, the same "no rows in, one row out" behaviour
/// [`super::aggregate::build_aggregate_sql`] relies on: a selection matching
/// nothing yields `accepted_count = 0` and a zero (or `NULL` `latest`) summary,
/// with no branch here for "the range selected nothing" versus "everything the
/// range selected was withdrawn".
///
/// # Errors
///
/// Returns the translation error string when `scope` names a field outside
/// the allowlist, uses an unsupported operator, or carries a value that
/// cannot be bound.
pub fn build_reconciliation_summary_sql(
    gts_type_uuid: Uuid,
    time_range: TimeRange,
    tenant_id: Uuid,
    fold: AggregationFold,
    scope: &ast::Expr,
) -> Result<ReconciliationSummaryStatement, String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_and_range_clauses(gts_type_uuid, time_range, &mut ctx, &mut clauses);
    let tenant_n = ctx.push(SqlBind::Uuid(tenant_id));
    clauses.push(format!("r.tenant_id = ${tenant_n}"));
    clauses.push(translate_scope(scope, &mut ctx)?);

    let select_list = if QuantitySummary::accrues(fold) {
        format!("COUNT(*), {}", accrued_summary_select_expr())
    } else {
        format!(
            "COUNT(*), {}, {}",
            observation_count_select_expr(),
            observation_latest_select_expr()
        )
    };

    Ok(ReconciliationSummaryStatement {
        sql: format!(
            "SELECT {select_list} FROM {} WHERE {}",
            ledger_from_clause(),
            clauses.join(" AND "),
        ),
        binds: ctx.binds,
    })
}

/// One assembled S3 statement (spec §9.3): the two watermarks, unbounded by
/// any range.
#[derive(Debug)]
pub struct ReconciliationWatermarksStatement {
    pub sql: String,
    pub binds: Vec<SqlBind>,
}

/// Build spec §9.3's S3 — `max(accepted_at)` and `max(window_end)` over the
/// scope alone, with **no range predicate** — as two scalar subqueries
/// sharing one `WHERE`, rather than folding either into
/// [`build_reconciliation_summary_sql`]'s ranged scan.
///
/// ```sql
/// SELECT
///   (SELECT r.accepted_at FROM usage_records r WHERE <scope> ORDER BY r.accepted_at DESC LIMIT 1),
///   (SELECT r.window_end  FROM usage_records r WHERE <scope> ORDER BY r.window_end  DESC LIMIT 1)
/// ```
///
/// **Two scalar subqueries, not one aggregate with two `MAX`s, and not folded
/// into S1∧S2's `WHERE`.** Both watermarks are unbounded by the request's range
/// (DESIGN §3.6: "… unbounded, regardless of range"), so adding them to
/// [`build_reconciliation_summary_sql`]'s ranged `WHERE` would force an unranged
/// `FROM` there too — a full scan of the scope's history. Each subquery is an
/// ordinary `ORDER BY … LIMIT 1`, which makes the index reachable however the
/// planner would rewrite a bare `MAX(…)`: `usage_records_watermark_idx
/// (gts_type_uuid, tenant_id, accepted_at DESC)` serves `max_accepted_at` and
/// `usage_records_tenant_type_window_idx (tenant_id, gts_type_uuid, window_end
/// DESC)` serves `max_window_end`, both leading on exactly the columns
/// [`push_meter_clause`] and the tenant predicate below fix.
///
/// The `WHERE` — the meter identity alone (no range: [`push_meter_clause`], not
/// [`push_meter_and_range_clauses`]), the tenant, and the compiled scope — is
/// built once and spliced into both subqueries verbatim, so every placeholder it
/// binds is written twice in the rendered SQL but bound once.
///
/// A scope, tenant or type this backend holds no entry under leaves both
/// subqueries at zero rows, so both columns decode as `NULL` —
/// `max_accepted_at` and `max_window_end` both `None`, which is
/// [`usage_collector_sdk::ReconciliationMetadata::empty_for`]'s watermark half.
///
/// # Errors
///
/// Returns the translation error string when `scope` names a field outside
/// the allowlist, uses an unsupported operator, or carries a value that
/// cannot be bound.
pub fn build_reconciliation_watermarks_sql(
    gts_type_uuid: Uuid,
    tenant_id: Uuid,
    scope: &ast::Expr,
) -> Result<ReconciliationWatermarksStatement, String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_clause(gts_type_uuid, &mut ctx, &mut clauses);
    let tenant_n = ctx.push(SqlBind::Uuid(tenant_id));
    clauses.push(format!("r.tenant_id = ${tenant_n}"));
    clauses.push(translate_scope(scope, &mut ctx)?);
    let where_sql = clauses.join(" AND ");
    let from = ledger_from_clause();

    Ok(ReconciliationWatermarksStatement {
        sql: format!(
            "SELECT \
             (SELECT r.accepted_at FROM {from} WHERE {where_sql} \
              ORDER BY r.accepted_at DESC LIMIT 1), \
             (SELECT r.window_end FROM {from} WHERE {where_sql} \
              ORDER BY r.window_end DESC LIMIT 1)"
        ),
        binds: ctx.binds,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "reconciliation_tests.rs"]
mod reconciliation_tests;
