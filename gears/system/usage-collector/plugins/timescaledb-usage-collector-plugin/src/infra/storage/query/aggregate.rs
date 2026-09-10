//! Aggregation SQL: inject-safe SELECT-expression and WHERE-clause builders for
//! the pushed-down `aggregate` query.
//!
//! These fragments compose into
//! `SELECT <dim exprs…>, <FOLD> FROM usage_records r WHERE <scope> AND
//! <withdrawal exclusion> [AND …] [GROUP BY 1, 2, …]`. The `FROM` and the scope
//! predicates are the caller's — including the `r` alias, which is a stated
//! precondition rather than something this module can emit. The builders here
//! own the rest:
//!
//! - [`agg_select_expr`] — the aggregate column for a fold that is an aggregate
//!   function.
//! - [`latest_select_expr`] — the ordered pick [`AggregationFold::Latest`] is
//!   instead.
//! - [`withdrawal_exclusion_clause`] — the two obligations a withdrawn pair
//!   places on every fold.
//! - [`dimension_select_expr`] — a group dimension as a TEXT-returning expr.
//! - [`aggregate_limit_clause`] — the distinct-group cardinality bound.
//!
//! Every fragment that names a column qualifies it with `r`, the alias the
//! caller MUST give `usage_records`. Nothing in the crate enforces that; see
//! the precondition on [`withdrawal_exclusion_clause`].
//!
//! All identifiers come from the closed [`AggregationFold`] /
//! [`AggregationDimension`] enum matches (an allowlist — never caller text), so
//! no identifier is interpolated from untrusted input. The only caller-derived
//! value, a [`AggregationDimension::Metadata`] key, is bound (`$N`) via the
//! shared [`SqlCtx`].

use usage_collector_sdk::{AggregationDimension, AggregationFold, MAX_AGGREGATION_BUCKETS};

use super::bind::SqlBind;
use super::translate::SqlCtx;

/// SQL aggregate expression for an [`AggregationFold`], or `None` when the fold
/// is not an aggregate function.
///
/// Every fold casts to `numeric` so the result — including the integer-typed
/// `COUNT(*)` — reads back uniformly as `Option<BigDecimal>` in `aggregate`.
/// Reading into arbitrary-precision `bigdecimal::BigDecimal` (the type of the
/// SDK's `AggregationBucket.value`) is why a wide `SUM` no longer hits
/// `rust_decimal::Decimal`'s ceiling of roughly 7.9e28 and turns into an
/// `Internal`/500 on decode.
///
/// [`AggregationFold::Latest`] has no arm: it is not an aggregate function but
/// an ordered pick, rendered by [`latest_select_expr`]. Its `None` means
/// "rendered elsewhere", never "unsupported fold" — a caller that treats it as
/// the latter serves no `LATEST` meter at all.
///
/// The returned string is a `'static` constant from the closed enum match,
/// never caller text. Every arm that names a column qualifies it with the
/// caller's `r` alias; `COUNT(*)` names none.
#[must_use]
pub fn agg_select_expr(fold: AggregationFold) -> Option<&'static str> {
    match fold {
        AggregationFold::Sum => Some("SUM(r.value)::numeric"),
        AggregationFold::Count => Some("COUNT(*)::numeric"),
        AggregationFold::Min => Some("MIN(r.value)::numeric"),
        AggregationFold::Max => Some("MAX(r.value)::numeric"),
        AggregationFold::Latest => None,
    }
}

/// SQL expression for [`AggregationFold::Latest`], on the declared tie-break.
///
/// DESIGN §3.1 declares the rule as *greatest `window_end`, then greatest
/// `acceptance_sequence`*, and gives the termination argument as the sequence
/// being monotonic inside the group's scope — it is strictly monotonic per
/// `(tenant_id, gts_type_id)`, restated as a storage obligation in DESIGN §3.7.
/// A group narrower than that scope inherits the order; a group wider than one
/// tenant is outside the argument DESIGN makes, and no total order is claimed
/// across tenants. This backend can implement the declared rule exactly,
/// because it assigns and stores `acceptance_sequence` itself (DESIGN §3.7).
///
/// **That is worth stating because the SDK's own reference backend cannot.**
/// `UsageRecord` carries no such field, so `InMemoryReferencePlugin`
/// substitutes the greatest `id`, and the `latest-tie-break` contract check is
/// blocked for the same reason (it sits in the SDK's `BLOCKED_CHECKS`;
/// `DIVERGENCES.md` entries 10 and 19 carry the standing record). **A green
/// contract run therefore says nothing about this expression in either
/// direction.** Not merely because no check asserts the tie-break: the suite in
/// `usage-collector-sdk/src/contract/checks/` calls the aggregate method twice
/// and passes `AggregationFold::Sum` both times, so it never reaches this
/// expression at all. What pins it is this module's own tests.
///
/// `ARRAY_AGG(… ORDER BY …)[1]` renders the pick rather than `DISTINCT ON`
/// because it composes with a `GROUP BY` over arbitrary dimensions, which
/// `DISTINCT ON` does not: `DISTINCT ON` picks per distinct prefix of the
/// query's own `ORDER BY` and cannot be nested inside a grouped aggregate
/// SELECT list. A reader will otherwise reach for it.
///
/// The returned string is a `'static` constant, never caller text, and
/// qualifies its columns with the caller's `r` alias.
#[must_use]
pub fn latest_select_expr() -> &'static str {
    "(ARRAY_AGG(r.value ORDER BY r.window_end DESC, r.acceptance_sequence DESC))[1]::numeric"
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
/// *not* filter them; every other op filtered them out. An invalidation echoes
/// the quantity it withdraws rather than negating it, so netting would now
/// double-count, and the exception `SUM` used to enjoy is exactly the defect.
///
/// Obligation 1 standing alone is not pedantry: retention is plugin-owned
/// (DESIGN §3.10 "Consistency Contract"), so a conforming deployment can purge
/// a target and keep the invalidation that withdrew it. That orphan still
/// contributes nothing — admitting it would be the echoed quantity reported
/// with nothing left to pair it against.
///
/// The `invalidation-excluded-from-fold` contract check exercises this, but
/// under `AggregationFold::Sum` only — the whole suite passes no other fold —
/// so the property that makes this one clause rather than five, its holding
/// under every fold, is pinned here and nowhere else.
///
/// # Precondition
///
/// The caller MUST alias `usage_records` as `r` in the outer query's `FROM`.
/// `r` is what both conjuncts bind against, and the correlated subquery's own
/// `w` is what keeps `w.invalidates = r.id` unambiguous. A caller that aliases
/// differently, or omits the alias, produces invalid SQL at runtime with no
/// compile error; nothing in the crate enforces it.
///
/// The returned string is a `'static` constant, never caller text.
#[must_use]
pub fn withdrawal_exclusion_clause() -> &'static str {
    "r.invalidates IS NULL \
     AND NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id)"
}

/// SQL TEXT-returning expression for a group [`AggregationDimension`].
///
/// The identity columns map through the closed enum match (an allowlist), so
/// the only caller-derived value — the [`AggregationDimension::Metadata`] key —
/// is bound via `ctx` (`r.metadata ->> $N`) rather than interpolated.
/// `tenant_id` is a `uuid` column, so it is cast to `text` for a uniform
/// `Option<String>` positional read in `aggregate`. Every column carries the
/// caller's `r` alias, matching [`withdrawal_exclusion_clause`].
///
/// The variant set is the SDK's, and it is five fixed dimensions plus the
/// metadata escape hatch where DESIGN gives `group_by` eight fixed ones;
/// `entry_type`, `origin` and `invalidates` have no variant to render.
/// `DIVERGENCES.md` entry 15 holds that gap open, and growing the enum is not
/// this backend's to do.
///
/// # The absent-dimension rule, which lives at the caller
///
/// A row whose dimension column is `NULL` — no `subject_ref`, or a metadata key
/// absent from the row — has no dimension value to group by, and this file
/// cannot say which answer the backend gives: the aggregate caller today guards
/// the two subject dimensions with an `IS NOT NULL` predicate and guards
/// [`AggregationDimension::Metadata`] with nothing, so metadata is where the
/// two backends actually differ. What this function returns is a bare column,
/// which in a SQL `GROUP BY` **collects** such rows into a `NULL` group;
/// `InMemoryReferencePlugin` **drops** them, for every dimension including an
/// absent metadata key.
///
/// `DIVERGENCES.md` §G raised this as a spec question rather than a check, and
/// **the spec owner has since settled it: drop the row.** That matches the
/// reference backend, and it matches what the SDK had already documented on
/// [`AggregationDimension::SubjectId`] and
/// [`AggregationDimension::SubjectType`] — rows without a subject are excluded
/// from the grouping — which left only [`AggregationDimension::Metadata`]
/// unstated. It also fits the published wire shape, where
/// `AggregationBucket.key` types every item as a non-nullable string with no
/// null spelling available.
///
/// The consequence, now uniform rather than accidental: **grouped buckets need
/// not sum to the ungrouped total.**
///
/// The guard is the caller's to emit, not this expression's — a `GROUP BY`
/// ordinal cannot carry a `WHERE` predicate — so nothing changes here and no
/// test in this module pins the rule. The caller owes a not-null predicate for
/// every dimension it renders, `Metadata` included.
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
/// The no-grouping case (`dim_count == 0`) is a single aggregate row and needs no
/// cap, so it yields an empty clause.
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
