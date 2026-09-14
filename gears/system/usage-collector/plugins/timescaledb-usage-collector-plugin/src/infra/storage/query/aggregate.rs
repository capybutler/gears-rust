//! Aggregation SQL: inject-safe SQL-fragment builders for the pushed-down
//! `aggregate` query.
//!
//! The fragments compose into `SELECT <dim exprs…>, <fold expr> FROM <from
//! clause> WHERE <scope> AND <withdrawal exclusion> [AND …] [GROUP BY 1, 2, …]
//! [LIMIT …]`. The `FROM` clause is not this module's: it is
//! [`super::ledger_from_clause`], shared with the list path, and every fragment
//! below qualifies its columns with the `r` alias that clause declares. Only
//! the scope predicates are the caller's:
//!
//! - [`fold_select_expr`] — the folded column, one arm per fold.
//! - [`withdrawal_exclusion_clause`] — the two obligations a withdrawn pair
//!   places on every fold.
//! - [`dimension_select_expr`] — a group dimension as a TEXT-returning expr.
//! - [`dimension_presence_guard`] — the `WHERE` half of that dimension, for the
//!   three whose column can be `NULL`.
//! - [`aggregate_limit_clause`] — the distinct-group cardinality bound.
//!
//! Every identifier comes from a closed enum match (an allowlist), never from
//! caller text. The one caller-derived value, a
//! [`AggregationDimension::Metadata`] key, is bound (`$N`) via [`SqlCtx`].

use usage_collector_sdk::{AggregationDimension, AggregationFold, MAX_AGGREGATION_BUCKETS};

use super::bind::SqlBind;
use super::translate::SqlCtx;

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
///
/// Both `DISTINCT ON` and a `ROW_NUMBER()` window have since been **measured**
/// against this form rather than only argued about, and both stay out. The
/// decisive fact is not the memory, and belongs here rather than behind a link:
/// **neither can express the ungrouped fold.** `DISTINCT ON ()` is a syntax
/// error, and `PARTITION BY` nothing - like the `ORDER BY … LIMIT 1` rewrite -
/// answers **zero** rows over an empty selection, where the SPI owes exactly
/// one empty-keyed bucket. Adopting either therefore means a second statement
/// shape for this one fold, carrying its own empty-selection special case.
/// The measurement itself - ~25 MB saved on a single-group worst case, and why
/// that does not buy the composition back - is on
/// [`super::super::record_store::PgRecordStore`]'s `aggregate`.
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
/// [`super::ledger_from_clause`] declares; `COUNT(*)` names none.
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
/// The subquery also pins `w.type_key = r.type_key`. An invalidation has its
/// target's type and so its key, so this excludes nothing the `id` match would
/// keep; what it buys is chunk exclusion inside the subquery, which otherwise
/// probes the invalidation index in every type's chunks.
///
/// # Precondition
///
/// The outer query's `FROM` must be [`super::ledger_from_clause`] — `r` is what
/// both conjuncts bind against, and the subquery's own `w` is what keeps
/// `w.invalidates = r.id` unambiguous. The returned string is a `'static`
/// constant, never caller text.
#[must_use]
pub fn withdrawal_exclusion_clause() -> &'static str {
    "r.invalidates IS NULL \
     AND NOT EXISTS (SELECT 1 FROM usage_records w \
     WHERE w.invalidates = r.id AND w.type_key = r.type_key)"
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
/// a `NULL` group. The rule is to drop the row instead — what the SDK
/// documents at `models.rs:1587-1592` and what its reference backend does at
/// `contract/reference.rs:801`, not a ruling recorded in `DIVERGENCES.md` §G,
/// which still holds the question open for a spec owner (`DIVERGENCES.md` §G,
/// resolved by this port's Task 18). Enforcing it is the caller's,
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

/// The presence guard a grouped dimension needs so a row missing that
/// dimension is dropped rather than folded into a `NULL` bucket.
///
/// Built from `select_expr` — the very string the `GROUP BY` ordinal points
/// at — so it is by construction the exact negation of "the grouping
/// expression yields `NULL`" and cannot drift from the expression it guards.
/// For a metadata dimension that reads `r.metadata ->> $N IS NOT NULL`, whose
/// bound key is the one [`dimension_select_expr`] already pushed; the key is
/// therefore bound once, not twice.
///
/// `IS NOT NULL` over `->>` rather than the containment operator `?`: they
/// disagree on a key present with JSON `null`, where `?` is true and `->>` is
/// `NULL`, and it is `NULL` that decides the bucket. (Where containment really
/// is wanted the unambiguous spelling is `jsonb_exists(r.metadata, $N)`; a bare
/// `?` collides with the placeholder syntax of some drivers.)
///
/// `None` for the three columns the schema declares `NOT NULL` — a guard on
/// them would be dead SQL. The match is exhaustive on purpose: a new
/// [`AggregationDimension`] variant fails to compile here, next to the decision
/// it needs.
///
/// **The consequence is deliberate: grouped buckets need not sum to the
/// ungrouped total.** Dropping the row is what the SDK both documents and
/// does: `models.rs:1587-1592` says rows without a subject "are excluded from
/// the grouping", and `contract/reference.rs:801` reads a grouped metadata key
/// as `row.metadata.get(key).cloned()`, so an absent key yields `None` and the
/// row joins no bucket.
///
/// **`DIVERGENCES.md` §G is not the citation for this**, though it is where a
/// reader will look. §G records the question as still *open* — "DESIGN says
/// nothing about the case", "that is an argument, not a ruling ... and it is a
/// spec owner's to make", "do not write the check first". The ruling is this
/// port's Task 18, which rewrites §G; until then the two SDK sites above are
/// what this guard conforms to (`DIVERGENCES.md` §G, resolved by Task 18).
///
/// It was already true of the two subject dimensions; guarding the metadata one
/// makes it uniform rather than accidental.
#[must_use]
pub fn dimension_presence_guard(dim: &AggregationDimension, select_expr: &str) -> Option<String> {
    match dim {
        AggregationDimension::SubjectId
        | AggregationDimension::SubjectType
        | AggregationDimension::Metadata(_) => Some(format!("{select_expr} IS NOT NULL")),
        AggregationDimension::TenantId
        | AggregationDimension::ResourceId
        | AggregationDimension::ResourceType => None,
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
