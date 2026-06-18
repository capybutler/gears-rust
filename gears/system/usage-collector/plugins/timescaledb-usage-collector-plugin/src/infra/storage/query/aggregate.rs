//! Aggregation SQL (Phase 4): inject-safe SELECT-expression builders for the
//! pushed-down `aggregate` query.
//!
//! `aggregate` assembles `SELECT <dim exprs…>, <AGG> FROM usage_records WHERE
//! gts_id = $1 AND status = 'active' [AND …] [GROUP BY 1, 2, …]`. The two
//! helpers here own the two kinds of SELECT expression:
//!
//! - [`agg_select_expr`] — the aggregate column. Every variant casts to
//!   `numeric` (`COUNT(*)::numeric`, `SUM(value)::numeric`, `MIN/MAX/AVG(value)::numeric`)
//!   so the result reads back uniformly as `Option<Decimal>` regardless of the
//!   chosen op. `AVG` read-back assumes the average fits `rust_decimal::Decimal`
//!   (~28 significant digits), which holds for realistic usage quantities.
//! - [`dimension_select_expr`] — a group dimension as a TEXT-returning expr.
//!
//! All identifiers come from the closed [`AggregationOp`] /
//! [`AggregationDimension`] enum matches (an allowlist — never caller text), so
//! no identifier is interpolated from untrusted input. The only caller-derived
//! value, a [`AggregationDimension::Metadata`] key, is bound (`$N`) via the
//! shared [`SqlCtx`].

use usage_collector_sdk::{AggregationDimension, AggregationOp};

use super::bind::SqlBind;
use super::translate::SqlCtx;

/// SQL aggregate expression for an [`AggregationOp`].
///
/// Every op casts to `numeric` so the result — including the integer-typed
/// `COUNT(*)` — reads back uniformly as `Option<Decimal>` in `aggregate`. The
/// returned string is a `'static` constant from the closed enum match, never
/// caller text.
#[must_use]
pub fn agg_select_expr(op: AggregationOp) -> &'static str {
    match op {
        AggregationOp::Sum => "SUM(value)::numeric",
        AggregationOp::Count => "COUNT(*)::numeric",
        AggregationOp::Min => "MIN(value)::numeric",
        AggregationOp::Max => "MAX(value)::numeric",
        AggregationOp::Avg => "AVG(value)::numeric",
    }
}

/// SQL TEXT-returning expression for a group [`AggregationDimension`].
///
/// The identity columns map through the closed enum match (an allowlist), so
/// the only caller-derived value — the [`AggregationDimension::Metadata`] key —
/// is bound via `ctx` (`metadata ->> $N`) rather than interpolated. `tenant_id`
/// is a `uuid` column, so it is cast to `text` for a uniform `Option<String>`
/// positional read in `aggregate`.
///
/// Returns the SELECT expression string (used positionally; the `GROUP BY`
/// references it by ordinal so the bound metadata expr is never repeated).
pub fn dimension_select_expr(dim: &AggregationDimension, ctx: &mut SqlCtx) -> String {
    match dim {
        AggregationDimension::TenantId => "tenant_id::text".to_owned(),
        AggregationDimension::ResourceId => "resource_id".to_owned(),
        AggregationDimension::ResourceType => "resource_type".to_owned(),
        AggregationDimension::SubjectId => "subject_id".to_owned(),
        AggregationDimension::SubjectType => "subject_type".to_owned(),
        AggregationDimension::Metadata(key) => {
            let n = ctx.push(SqlBind::Str(key.as_str().to_owned()));
            format!("metadata ->> ${n}")
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "aggregate_tests.rs"]
mod aggregate_tests;
