//! Aggregation SQL: inject-safe SQL-fragment builders for the pushed-down
//! `aggregate` query.
//!
//! The fragments compose into `SELECT <dim exprs…>, <fold expr> FROM <from
//! clause> WHERE <scope> AND <withdrawal exclusion> [AND …] [GROUP BY 1, 2, …]
//! [LIMIT …]`. Only the scope predicates are the caller's:
//!
//! - [`aggregate_from_clause`] — the ledger table and the `r` alias every other
//!   fragment qualifies its columns with.
//! - [`fold_select_expr`] — the folded column, one arm per fold.
//! - [`withdrawal_exclusion_clause`] — the two obligations a withdrawn pair
//!   places on every fold.
//! - [`dimension_select_expr`] — a group dimension as a TEXT-returning expr.
//! - [`aggregate_limit_clause`] — the distinct-group cardinality bound.
//!
//! Every identifier comes from a closed enum match (an allowlist), never from
//! caller text. The one caller-derived value, a
//! [`AggregationDimension::Metadata`] key, is bound (`$N`) via [`SqlCtx`].

use usage_collector_sdk::{AggregationDimension, AggregationFold, MAX_AGGREGATION_BUCKETS};

use super::bind::SqlBind;
use super::translate::SqlCtx;

/// The aggregate query's `FROM` clause: the ledger table and the alias every
/// other fragment here binds to.
///
/// It exists so the alias is one constant both sides read rather than a
/// convention two files independently honour — a caller spelling its own `FROM`
/// can pick a different one with no compile error and invalid SQL at runtime.
#[must_use]
pub fn aggregate_from_clause() -> &'static str {
    "usage_records r"
}

/// The [`AggregationFold::Latest`] arm of [`fold_select_expr`].
///
/// DESIGN §3.1 declares the rule as *greatest `window_end`, then greatest
/// `acceptance_sequence`*, terminating because the sequence is monotonic inside
/// the group's scope — strictly so per `(tenant_id, gts_type_id)` (DESIGN §3.7).
/// A narrower group inherits that order; a group spanning tenants is outside the
/// argument, and no cross-tenant total order is claimed.
///
/// This backend implements the declared rule exactly, because it assigns
/// `acceptance_sequence` itself. **The SDK's reference backend cannot:**
/// `UsageRecord` has no such field, so `InMemoryReferencePlugin` substitutes the
/// greatest `id` and `latest-tie-break` sits in the SDK's `BLOCKED_CHECKS`
/// (`DIVERGENCES.md` entries 10 and 19). **No contract check asserts this
/// expression either way;** this module's tests are what pin it.
///
/// `LATEST` is an ordered pick, not an aggregate function — but
/// `ARRAY_AGG(… ORDER BY …)[1]` composes in a grouped SELECT list exactly as
/// `SUM(…)` does, so the caller needs no separate shape for it. `DISTINCT ON`
/// does not compose that way (it picks per distinct prefix of the query's own
/// `ORDER BY`), and is named here because a reader will otherwise reach for it.
const LATEST_SELECT_EXPR: &str =
    "(ARRAY_AGG(r.value ORDER BY r.window_end DESC, r.acceptance_sequence DESC))[1]::numeric";

/// SQL expression folding the selected rows, one arm per [`AggregationFold`].
///
/// Every fold casts to `numeric` so the result — including the integer-typed
/// `COUNT(*)` — reads back uniformly as `Option<BigDecimal>`. That
/// arbitrary-precision type (the SDK's `AggregationBucket.value`) is why a wide
/// `SUM` no longer hits `rust_decimal::Decimal`'s ceiling of roughly 7.9e28 and
/// turns into an `Internal`/500 on decode.
///
/// The returned string is a `'static` constant from the closed enum match,
/// never caller text. Each arm naming a column qualifies it with the alias
/// [`aggregate_from_clause`] declares; `COUNT(*)` names none.
#[must_use]
pub fn fold_select_expr(fold: AggregationFold) -> &'static str {
    match fold {
        AggregationFold::Sum => "SUM(r.value)::numeric",
        AggregationFold::Count => "COUNT(*)::numeric",
        AggregationFold::Min => "MIN(r.value)::numeric",
        AggregationFold::Max => "MAX(r.value)::numeric",
        AggregationFold::Latest => LATEST_SELECT_EXPR,
    }
}

/// Withdrawal-exclusion WHERE clause. One rule, under every fold.
///
/// The SPI states two obligations rather than one conditional
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`), and both hold under
/// every [`AggregationFold`], so there is no per-fold branch here:
///
/// 1. An **invalidation entry** contributes nothing to any fold, whether or not
///    its target is in the selection — the `r.invalidates IS NULL` conjunct.
/// 2. A **record an accepted invalidation names** contributes nothing either —
///    the `NOT EXISTS` conjunct.
///
/// **This is a change of rule, not only of spelling.** The retired `corrects_id`
/// model had `SUM` net across signed compensation rows and so deliberately did
/// *not* filter them. An invalidation echoes the quantity it withdraws rather
/// than negating it, so netting would now double-count: the exception `SUM` used
/// to enjoy is exactly the defect.
///
/// Obligation 1 standing alone is not pedantry. Retention is plugin-owned
/// (DESIGN §3.10 "Consistency Contract"), so a conforming deployment can purge a
/// target and keep the invalidation that withdrew it; that orphan is the echoed
/// quantity with nothing left to pair it against.
///
/// # Precondition
///
/// The outer query's `FROM` must be [`aggregate_from_clause`] — `r` is what both
/// conjuncts bind against, and the subquery's own `w` is what keeps
/// `w.invalidates = r.id` unambiguous. The returned string is a `'static`
/// constant, never caller text.
#[must_use]
pub fn withdrawal_exclusion_clause() -> &'static str {
    "r.invalidates IS NULL \
     AND NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id)"
}

/// SQL TEXT-returning expression for a group [`AggregationDimension`].
///
/// The identity columns map through the closed enum match (an allowlist), so
/// the one caller-derived value — the [`AggregationDimension::Metadata`] key —
/// is bound via `ctx` (`r.metadata ->> $N`) rather than interpolated.
/// `tenant_id` is a `uuid`, cast to `text` for a uniform `Option<String>` read.
///
/// The variant set is the SDK's: five fixed dimensions plus the metadata escape
/// hatch, where DESIGN gives `group_by` eight fixed ones. `DIVERGENCES.md`
/// entry 15 holds that gap open; growing the enum is not this backend's to do.
///
/// **Absent dimensions.** This returns a bare column, and a `NULL` in one forms
/// a `NULL` group. The rule is to drop the row instead — a spec owner's
/// decision, recorded at `DIVERGENCES.md` §G — and enforcing it is the caller's,
/// since a `GROUP BY` ordinal carries no `WHERE` predicate. Only `subject_id`,
/// `subject_type` and an absent metadata key can be `NULL`; the other columns
/// are `NOT NULL` in the schema, so a guard on them would be dead SQL.
///
/// Returns the SELECT expression string (used positionally; the `GROUP BY`
/// references it by ordinal so the bound metadata expr is never repeated).
pub fn dimension_select_expr(dim: &AggregationDimension, ctx: &mut SqlCtx) -> String {
    match dim {
        AggregationDimension::TenantId => "r.tenant_id::text".to_owned(),
        AggregationDimension::ResourceId => "r.resource_id".to_owned(),
        AggregationDimension::ResourceType => "r.resource_type".to_owned(),
        AggregationDimension::SubjectId => "r.subject_id".to_owned(),
        AggregationDimension::SubjectType => "r.subject_type".to_owned(),
        AggregationDimension::Metadata(key) => {
            let n = ctx.push(SqlBind::Str(key.as_str().to_owned()));
            format!("r.metadata ->> ${n}")
        }
    }
}

/// `LIMIT` clause bounding the aggregate's distinct-group cardinality.
///
/// The gateway-enforced bounded window caps the rows scanned; it does not cap
/// the distinct groups a `GROUP BY` produces. A high-cardinality
/// [`AggregationDimension::Metadata`] key (e.g. a per-record id) could otherwise
/// materialize an unbounded bucket set into memory, unlike the page-size-clamped
/// list path. `LIMIT MAX_AGGREGATION_BUCKETS + 1` bounds that; the `+ 1` lets the
/// gateway tell "exactly at the cap" from "over the cap" and reject the latter
/// with a `400` ([`usage_collector_sdk::reason::AGGREGATION_RESULT_TOO_LARGE`]).
/// With no grouping (`dim_count == 0`) there is one aggregate row and no
/// cardinality to bound, so the clause is empty.
///
/// It bounds *groups*, not rows per group: [`LATEST_SELECT_EXPR`] materializes a
/// group's values before picking one, and nothing here caps that.
#[must_use]
pub fn aggregate_limit_clause(dim_count: usize) -> String {
    if dim_count == 0 {
        String::new()
    } else {
        format!(" LIMIT {}", MAX_AGGREGATION_BUCKETS + 1)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "aggregate_tests.rs"]
mod aggregate_tests;
