//! Aggregation SQL for the pushed-down `aggregate` query: the inject-safe
//! fragment builders, and [`build_aggregate_sql`], which composes them into the
//! exact-scan statement.
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
//!   dimensions whose column can be `NULL`.
//! - [`aggregate_limit_clause`] — the distinct-group cardinality bound.
//!
//! Every identifier comes from a closed enum match (an allowlist), never from
//! caller text. The one caller-derived value, a
//! [`AggregationDimension::Metadata`] key, is bound (`$N`) via [`SqlCtx`].
//!
//! **The assembly lives here too, beside the fragments it composes**, the
//! shape [`super::feed`] and [`super::rollup`] already take: one module owns a
//! path's statement end to end, so no reader has to hold two files open to see
//! which fragment landed in which clause. [`AggregateStatement`] is what both
//! this path and [`super::rollup`] return, so the two aggregate routes are one
//! type rather than two identical ones repacked at the call site.
//!
//! What is **not** here: anything needing a connection or a `sqlx` type. The
//! pool acquire, the rollup eligibility routing, the metrics and the decode of
//! the rows the statement produces are all
//! [`super::super::record_store`]'s `aggregate`. [`AggregateStatement::dim_count`]
//! is what carries the one fact that decode cannot re-derive.

use toolkit_odata::ODataQuery;
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, MAX_AGGREGATION_BUCKETS, MetadataFilter, TimeRange,
};

use super::bind::SqlBind;
use super::translate::{SqlCtx, translate_scope};
use super::{ledger_from_clause, push_metadata_filter_clauses, push_meter_and_range_clauses};

/// The [`AggregationFold::Latest`] arm of [`fold_select_expr`].
///
/// **The last key is what makes the order total.** The gear's DESIGN §3.1 states
/// the `LATEST` tie-break as *"Greatest `window_end`, then greatest
/// `accepted_at`, then greatest `id` in byte order"*, adding that `id` is unique
/// so the order is total and that all its keys compare across tenants and types.
/// This plugin's own DESIGN §3.6 states the same rule: *"`LATEST` orders by
/// `window_end DESC, accepted_at DESC, id DESC`, a total order across tenants."*
/// This expression is that, key for key.
///
/// Every key is a stored column of the row being folded, so none of this rests
/// on an ordering the plugin assigns. `id` is the `UUIDv5` entry identity and
/// `uuid` comparison in `PostgreSQL` is over the bytes, which is the byte order
/// §3.1 names. Nothing here is scoped: a group spanning tenants or types is
/// ordered by the same keys as a group inside one scope.
///
/// **No key is droppable.** `aggregate_tests.rs`'s
/// `latest_picks_the_greatest_window_end_then_accepted_at_then_id` pins the
/// exact three-key `ORDER BY` as a SQL string literal, which makes a
/// reordering visible but proves nothing about how `PostgreSQL` resolves a
/// tie against real rows.
///
/// `LATEST` is an ordered pick, not an aggregate function — but
/// `ARRAY_AGG(… ORDER BY …)[1]` composes in a grouped SELECT list exactly as
/// `SUM(…)` does, so the caller needs no separate shape for it. `DISTINCT ON`
/// does not compose that way (it picks per distinct prefix of the query's own
/// `ORDER BY`), and is named here because a reader will otherwise reach for it.
///
/// Both `DISTINCT ON` and a `ROW_NUMBER()` window stay out, and the decisive
/// fact is not memory: **neither can express the ungrouped fold.**
/// `DISTINCT ON ()` is a syntax error, and `PARTITION BY` nothing — like the
/// `ORDER BY … LIMIT 1` rewrite — answers **zero** rows over an empty selection,
/// where the SPI owes exactly one empty-keyed bucket. Adopting either therefore
/// means a second statement shape for this one fold, carrying its own
/// empty-selection special case. The memory measurement is on
/// [`super::super::record_store::PgRecordStore`]'s `aggregate`.
///
/// `pub(super)` rather than private: [`super::reconciliation`]'s
/// `observation_latest_select_expr` composes this exact expression with a
/// `FILTER` clause for the reconciliation summary's `Observations::latest`, so a
/// sibling module needs to read it rather than re-deriving the tie-break keys.
// @cpt-dod:cpt-cf-uc-plugin-dod-latest-total-order:p1
pub(super) const LATEST_SELECT_EXPR: &str = "(ARRAY_AGG(r.quantity ORDER BY r.window_end DESC, \
     r.accepted_at DESC, r.id DESC))[1]::numeric";

/// SQL expression folding the selected rows, one arm per [`AggregationFold`].
///
/// Every fold casts to `numeric` so the result — including the integer-typed
/// `COUNT(*)` — reads back uniformly as `Option<BigDecimal>`. That
/// arbitrary-precision type (the SDK's `AggregationBucket.value`) is why a wide
/// `SUM` no longer hits `rust_decimal::Decimal`'s ceiling of roughly 7.9e28 and
/// turns into an `Internal`/500 on decode.
///
/// **`SUM` folds through `COALESCE(…, 0)` and the others do not.** The gear's
/// DESIGN §3.3 ("An empty selection still answers") defines `SUM` and `COUNT`
/// over an empty selection at `0`, and leaves `MAX`, `MIN` and `LATEST`
/// undefined there, so those keep the `NULL` the backend answers and it reads
/// back as an absent value. `COUNT(*)` is already `0` over zero rows;
/// `PostgreSQL`'s `SUM` is `NULL`, which is the aggregate's own convention
/// rather than anything this fold means to say, so the wrapper supplies the
/// defined value. Two selections reach it: the ungrouped bucket of a query
/// matching no entry, and the ungrouped bucket of a range whose every entry is a
/// withdrawn pair, since [`withdrawal_exclusion_clause`] empties that selection
/// rather than removing the bucket. Grouped, the wrapper is inert — a group
/// exists only because a row survived into it.
///
/// The returned string is a `'static` constant from the closed enum match, never
/// caller text. Each arm naming a column qualifies it with the alias
/// [`super::ledger_from_clause`] declares; `COUNT(*)` names none.
// @cpt-dod:cpt-cf-uc-plugin-dod-empty-selection-values:p1
#[must_use]
pub fn fold_select_expr(fold: AggregationFold) -> &'static str {
    match fold {
        AggregationFold::Sum => "COALESCE(SUM(r.quantity), 0)::numeric",
        AggregationFold::Count => "COUNT(*)::numeric",
        AggregationFold::Min => "MIN(r.quantity)::numeric",
        AggregationFold::Max => "MAX(r.quantity)::numeric",
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
/// than negating it, so netting would now double-count.
///
/// Obligation 1 standing alone is not pedantry. Retention is plugin-owned
/// (DESIGN §3.10 "Consistency Contract"), so a conforming deployment can purge a
/// target and keep the invalidation that withdrew it; that orphan is the echoed
/// quantity with nothing left to pair it against.
///
/// The subquery also pins `w.type_key = r.type_key`. An invalidation has its
/// target's type and so its key, so this excludes nothing the `id` match would
/// keep; it restricts the subquery to the outer row's type, which lets the
/// planner skip other types' chunks where it can.
///
/// # Precondition
///
/// The outer query's `FROM` must be [`super::ledger_from_clause`] — `r` is what
/// both conjuncts bind against, and the subquery's own `w` is what keeps
/// `w.invalidates = r.id` unambiguous. The returned string is a `'static`
/// constant, never caller text.
// @cpt-dod:cpt-cf-uc-plugin-dod-withdrawn-pair-exclusion:p1
#[must_use]
pub fn withdrawal_exclusion_clause() -> &'static str {
    "r.invalidates IS NULL \
     AND NOT EXISTS (SELECT 1 FROM usage_records w \
     WHERE w.invalidates = r.id AND w.type_key = r.type_key)"
}

/// SQL TEXT-returning expression for a group [`AggregationDimension`].
///
/// The identity columns map through the closed enum match (an allowlist), so the
/// one caller-derived value — the [`AggregationDimension::Metadata`] key — is
/// bound via `ctx` (`r.metadata ->> $N`) rather than interpolated. `tenant_id` is
/// a `uuid`, cast to `text` for a uniform `Option<String>` read.
///
/// The variant set is the SDK's, which is also what DESIGN's
/// `AggregationDimension` row and `usage-collector-v1.yaml` publish — narrower
/// than the `$filter` set by design, not a deferred subset of it. Growing the
/// enum is still not this backend's to do.
///
/// **Absent dimensions.** This returns a bare column, and a `NULL` in one forms a
/// `NULL` group. The rule is to drop the row instead — what the SDK documents on
/// `AggregationDimension::SubjectId` / `SubjectType` (`models.rs`), not a spec
/// ruling. Enforcing it is the caller's, since a `GROUP BY` ordinal carries no
/// `WHERE` predicate. Only `subject_id`, `subject_type` and an absent metadata
/// key can be `NULL`; the other columns are `NOT NULL` in the schema, so a guard
/// on them would be dead SQL.
///
/// Returns the SELECT expression string (used positionally; the `GROUP BY`
/// references it by ordinal so the bound metadata expr is never repeated).
// @cpt-algo:cpt-cf-uc-plugin-algo-bucket-key-rendering:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-bucket-key-rendering:p1
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

/// The presence guard a grouped dimension needs so a row missing that dimension
/// is dropped rather than folded into a `NULL` bucket.
///
/// Built from `select_expr` — the very string the `GROUP BY` ordinal points at —
/// so it is by construction the exact negation of "the grouping expression yields
/// `NULL`" and cannot drift from the expression it guards. For a metadata
/// dimension that reads `r.metadata ->> $N IS NOT NULL`, whose bound key is the
/// one [`dimension_select_expr`] already pushed; the key is therefore bound once,
/// not twice.
///
/// `IS NOT NULL` over `->>` rather than the containment operator `?`: they
/// disagree on a key present with JSON `null`, where `?` is true and `->>` is
/// `NULL`, and it is `NULL` that decides the bucket. (Where containment really is
/// wanted the unambiguous spelling is `jsonb_exists(r.metadata, $N)`; a bare `?`
/// collides with the placeholder syntax of some drivers.)
///
/// `None` for the columns the schema declares `NOT NULL` — a guard on them would
/// be dead SQL. The match is exhaustive on purpose: a new
/// [`AggregationDimension`] variant fails to compile here, next to the decision
/// it needs.
///
/// **The consequence is deliberate: grouped buckets need not sum to the ungrouped
/// total.** Dropping the row is what the SDK documents:
/// `AggregationDimension::SubjectId` / `SubjectType` (`models.rs`) say rows
/// without a subject "are excluded from the grouping". **This guard conforms
/// to that SDK site, not to a spec ruling**: DESIGN says nothing about the
/// case.
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
/// The gateway-enforced bounded window caps the rows scanned; it does not cap the
/// distinct groups a `GROUP BY` produces. A high-cardinality
/// [`AggregationDimension::Metadata`] key (e.g. a per-record id) could otherwise
/// materialize an unbounded bucket set into memory, unlike the page-size-clamped
/// list path. `LIMIT MAX_AGGREGATION_BUCKETS + 1` bounds that; the `+ 1` lets the
/// gateway tell "exactly at the cap" from "over the cap" and reject the latter
/// with a `400` ([`usage_collector_sdk::reason::AGGREGATION_RESULT_TOO_LARGE`]).
/// With no grouping there is one aggregate row and no cardinality to bound, so
/// the clause is empty.
///
/// It bounds *groups*, not rows per group: the `LATEST` arm of
/// [`fold_select_expr`] materializes a group's values before picking one, and
/// nothing here caps that.
#[must_use]
pub fn aggregate_limit_clause(dim_count: usize) -> String {
    if dim_count == 0 {
        String::new()
    } else {
        format!(" LIMIT {}", MAX_AGGREGATION_BUCKETS + 1)
    }
}

/// One assembled aggregate statement: the SQL, the binds in placeholder order,
/// and **the dimension count the SELECT list was actually built from**.
///
/// Returned by both aggregate routes — [`build_aggregate_sql`]'s exact scan and
/// [`super::rollup::build_rollup_aggregate_sql`]'s rollup read — so the caller
/// picks a route and then runs one shape, rather than repacking one identical
/// struct into another.
///
/// `dim_count` is carried rather than recomputed by the caller: it is the count
/// the builder numbered the `GROUP BY` ordinals with and placed the fold after,
/// and it is what the decoder must read the same number of key columns with.
/// Re-derived at the call site the two could drift with nothing to notice — no
/// unit test executes a statement, so a decoder reading the wrong number of
/// columns is invisible until a live query.
///
/// `dim_count` is the one output of [`build_aggregate_sql`] that leaves no trace
/// in the SQL string, so no text oracle reaches it;
/// `the_builder_reports_the_dimension_count_its_select_list_was_built_from` is
/// what holds it. Contrast [`dimension_presence_guard`], which is correct by
/// construction: the guard string literally contains the select expression.
#[derive(Debug)]
pub struct AggregateStatement {
    pub sql: String,
    pub binds: Vec<SqlBind>,
    pub dim_count: usize,
}

/// Build the pushed-down aggregate statement and its binds, in placeholder
/// order.
///
/// ```sql
/// SELECT <dimension exprs…>, <fold expr>
/// FROM usage_records r
/// WHERE r.gts_type_uuid = $1 AND r.type_key = (…) AND r.window_end >= $2 AND r.window_end < $3
///   AND <withdrawal exclusion>
///   [AND <translated $filter>] [AND <metadata filters>] [AND <presence guards>]
/// [GROUP BY 1, 2, …] [LIMIT MAX_AGGREGATION_BUCKETS + 1]
/// ```
///
/// Every line but the last two comes from a shared builder rather than from a
/// second transcription here: [`ledger_from_clause`] is the `FROM`,
/// [`push_meter_and_range_clauses`] the meter and the covered-period range,
/// [`withdrawal_exclusion_clause`] the two obligations a withdrawn pair places
/// on every fold, and [`push_metadata_filter_clauses`] the side channel. Read
/// from one place, the range predicate cannot drift onto `window_start` in this
/// path alone.
///
/// The `$filter` goes through [`translate_scope`], which parenthesizes what it
/// returns. What arrives is the caller's filter `And`-composed with the compiled
/// PDP scope, **or the scope alone when the caller supplied none** — and a
/// multi-constraint grant compiles to a disjunction, so a bare fragment pushed
/// into a `join(" AND ")` would read as `(P AND A) OR B` and answer rows outside
/// the grant.
///
/// **No slot of `query` beyond `filter` is read.** This path paginates nothing
/// and mints no cursor, so `query.cursor`, `query.filter_hash` and `query.limit`
/// reach neither the statement nor the binds.
///
/// Identifiers all come from closed enum matches
/// ([`fold_select_expr`], [`dimension_select_expr`],
/// [`record_column`](super::translate::record_column)); the only caller-derived
/// values — a grouped metadata key, `$filter` operands, side channel keys and
/// values — are bound (`$N`).
///
/// # Errors
///
/// Returns the translation error string when the composed filter names a field
/// outside the allowlist, uses an unsupported operator, or carries a value that
/// cannot be bound.
// @cpt-algo:cpt-cf-uc-plugin-algo-exact-scan-fold:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-pushed-down-fold:p1
pub fn build_aggregate_sql(
    gts_type_uuid: Uuid,
    time_range: TimeRange,
    fold: AggregationFold,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
    group_by: &[AggregationDimension],
) -> Result<AggregateStatement, String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_and_range_clauses(gts_type_uuid, time_range, &mut ctx, &mut clauses);

    // The one rule this path applies that the ledger paths do not: an
    // invalidation entry contributes nothing, and neither does the record an
    // accepted invalidation names. Unconditional, because both obligations hold
    // under every fold.
    clauses.push(withdrawal_exclusion_clause().to_owned());

    if let Some(expr) = query.filter() {
        clauses.push(translate_scope(expr, &mut ctx)?);
    }

    push_metadata_filter_clauses(metadata_filter, &mut ctx, &mut clauses);

    // Dimension SELECT exprs in `GROUP BY` order, each binding its metadata key
    // at most once, each contributing a presence guard when its column can be
    // `NULL`.
    let mut select_dims: Vec<String> = Vec::with_capacity(group_by.len());
    for dim in group_by {
        let expr = dimension_select_expr(dim, &mut ctx);
        if let Some(guard) = dimension_presence_guard(dim, &expr) {
            clauses.push(guard);
        }
        select_dims.push(expr);
    }

    // With no dimensions the SELECT is the fold alone, which is what makes the
    // no-grouping case one aggregate row rather than none.
    let dim_count = select_dims.len();
    let mut select_parts = select_dims;
    select_parts.push(fold_select_expr(fold).to_owned());
    let select_list = select_parts.join(", ");

    // `GROUP BY` by ordinal (1..=k), so a bound metadata expr is written once
    // and the second reference cannot renumber its placeholder. Omitted
    // entirely with no dimensions: `GROUP BY` with an empty ordinal list is a
    // syntax error, and grouping by nothing is what a bare aggregate already
    // does.
    let group_by_sql = if dim_count == 0 {
        String::new()
    } else {
        let ordinals = (1..=dim_count)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(" GROUP BY {ordinals}")
    };

    Ok(AggregateStatement {
        sql: format!(
            "SELECT {select_list} FROM {} WHERE {}{group_by_sql}{}",
            // Called, never spelled: with a literal here the shared constant
            // would be decorative.
            ledger_from_clause(),
            clauses.join(" AND "),
            aggregate_limit_clause(dim_count),
        ),
        binds: ctx.binds,
        // The same count the SELECT list above was built from, handed to the
        // decoder rather than derived again there.
        dim_count,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "aggregate_tests.rs"]
mod aggregate_tests;
