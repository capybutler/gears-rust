//! Query read-path helpers for the usage-collector domain service.
//!
//! Holds the helpers that serve only the `list_usage_records` /
//! `query_aggregated_usage_records` read paths, kept out of `service.rs`
//! so it stays focused on orchestration:
//!
//! * `compose_query_with_scope` — AND-merges the PDP-returned
//!   [`AccessScope`] into the caller's `$filter`.
//! * `require_bounded_time_window` — rejects an unbounded `created_at`
//!   window before composition / dispatch.
//! * `reject_reserved_filter_fields` — rejects a `$filter` naming a field
//!   reserved to a typed parameter (`gts_type_id`, the covered-period
//!   bounds), wherever in the AST it appears.
//! * `require_dimensions_declared` — checks a `group_by` list only names
//!   the fixed dimensions or a metadata key the queried meter's resolved
//!   declaration actually declares.
//!
//! Per Spec §3.11, the admissible `$filter` / `group_by` surface is the
//! fixed fields plus the queried meter's declared metadata keys,
//! recomputed per request from the resolved declaration — never cached
//! independently of it, so a property declared a moment ago is usable on
//! the very next call.

use std::collections::BTreeSet;

use toolkit_odata::{ODataQuery, ast};
use toolkit_security::AccessScope;
use usage_collector_sdk::{AggregationDimension, MeterTypeId, UsageCollectorError};

use crate::domain::authz;

/// AND-merge the PDP-returned [`AccessScope`] into the caller's
/// [`ODataQuery`] filter under intersection-only semantics, returning a
/// fresh query ready for plugin dispatch.
///
/// `composed_filter = user_filter AND scope_filter`. The scope always
/// contributes a narrowing predicate: [`authz::scope_to_odata_filter`]
/// fails closed on an unconstrained / empty-constraint / deny-all scope
/// rather than yielding a pass-through, so there is no "filter unchanged"
/// branch. When the user supplied no filter the scope filter alone becomes
/// the composed filter. `gts_id`, the time window, and the order /
/// limit / cursor / select projections on [`ODataQuery`] flow through
/// verbatim — the composition only touches the `$filter` AST.
///
/// Per `cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2`:
/// composition is intersection-only (no widening). PDP constraint
/// shapes outside the supported set (tree predicates, unknown
/// properties, value-type mismatches) bubble up as fail-closed
/// [`AuthorizationDenied`](crate::domain::DomainError::AuthorizationDenied) from
/// [`authz::scope_to_odata_filter`].
// @cpt-algo:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2
pub(crate) fn compose_query_with_scope(
    user_query: &ODataQuery,
    scope: &AccessScope,
) -> Result<ODataQuery, UsageCollectorError> {
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-parse-pdp
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-iterate
    // `scope_to_odata_filter` always yields a narrowing predicate or fails
    // closed: an unconstrained / empty-constraint / deny-all scope is denied,
    // never passed through as "no row narrowing".
    let scope_expr = authz::scope_to_odata_filter(scope).map_err(UsageCollectorError::from)?;
    // @cpt-end:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-iterate
    // @cpt-end:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-parse-pdp

    // @cpt-begin:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-intersect
    let composed_filter: ast::Expr = match user_query.filter().cloned() {
        Some(user_expr) => user_expr.and(scope_expr),
        None => scope_expr,
    };
    // @cpt-end:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-intersect

    let mut composed = user_query.clone();
    // Preserve the caller's `filter_hash` (the hash of the *user* `$filter`,
    // computed by the gateway) — do NOT re-hash the AND-merged filter. The
    // keyset cursor's `f` field exists to detect the *user* changing their
    // `$filter` between paginated requests; the PDP scope AND-merged here is
    // server-injected and not user-controlled, so it MUST be excluded from the
    // hash. Both cursor validators (the gateway, pre-composition, and the
    // plugin's `cursor.f == query.filter_hash` check) compare against the
    // user-filter hash, and the plugin embeds `query.filter_hash` into the
    // next_cursor. Re-hashing to `hash(user AND scope)` here would embed a hash
    // the gateway's `hash(user)` can never match on the follow-up request —
    // breaking keyset pagination with a spurious `FILTER_MISMATCH` 400 the
    // moment PDP returns any row scope (latent until LIST began requiring
    // constraints). `composed` keeps `user_query.filter_hash` from the clone.
    composed.filter = Some(Box::new(composed_filter));
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-return
    Ok(composed)
    // @cpt-end:cpt-cf-usage-collector-algo-usage-query-pdp-constraint-composition-v2:p2:inst-constraint-composition-return
}

/// The `UsageRecord` field carrying the query time window. The bounded
/// `[from, to)` window is expressed in the `$filter` AST as
/// `created_at ge … and created_at lt …`.
const CREATED_AT_FIELD: &str = "created_at";

/// Require that `query`'s `$filter` pins a bounded `created_at` window:
/// at least one lower bound (`created_at ge|gt …`) **and** at least one
/// upper bound (`created_at le|lt …`) must appear as top-level
/// conjuncts. Without both, the query would drive an unbounded
/// full-table scan / aggregation — a `DoS` / cost footgun — so it is
/// rejected with [`UsageCollectorError::missing_time_window`].
///
/// Only **top-level conjuncts** count: a bound nested under an `or` /
/// `not` does not constrain the scan (rows outside the window can still
/// match), so the AND-chain is flattened and only its leaves are
/// inspected. The bound's value type is not checked here — a malformed
/// literal is a type mismatch caught downstream by the plugin's filter
/// conversion, not a missing-window error.
///
/// Enforced on the shared service path (not just the REST handler) so
/// in-process SDK callers and out-of-process REST callers obtain the
/// same guarantee, per the single-authorization-path contract.
pub(crate) fn require_bounded_time_window(query: &ODataQuery) -> Result<(), UsageCollectorError> {
    let Some(filter) = query.filter() else {
        return Err(UsageCollectorError::missing_time_window());
    };

    let mut has_lower = false;
    let mut has_upper = false;
    visit_top_level_conjuncts(filter, &mut |conjunct| {
        if let ast::Expr::Compare(left, op, _right) = conjunct
            && let ast::Expr::Identifier(name) = left.as_ref()
            && name.eq_ignore_ascii_case(CREATED_AT_FIELD)
        {
            match op {
                ast::CompareOperator::Ge | ast::CompareOperator::Gt => has_lower = true,
                ast::CompareOperator::Le | ast::CompareOperator::Lt => has_upper = true,
                _ => {}
            }
        }
    });

    if has_lower && has_upper {
        Ok(())
    } else {
        Err(UsageCollectorError::missing_time_window())
    }
}

/// Walk the top-level conjunction of `expr`, invoking `visit` on each
/// conjunct. `And` nodes are flattened transparently; any other node
/// (a leaf comparison, or an `or` / `not` / function subtree) is passed
/// to `visit` as a single opaque conjunct.
fn visit_top_level_conjuncts(expr: &ast::Expr, visit: &mut impl FnMut(&ast::Expr)) {
    match expr {
        ast::Expr::And(left, right) => {
            visit_top_level_conjuncts(left, visit);
            visit_top_level_conjuncts(right, visit);
        }
        other => visit(other),
    }
}

/// Field names a caller may never name in a `$filter`.
///
/// `gts_type_id` travels as a typed parameter and the covered period as a
/// typed time range (`window_start` / `window_end`, landing with the
/// record-model slice after this plan — reserved here regardless, so the
/// name is guarded against ever becoming filterable), so a predicate over
/// any of the three would express a second, possibly contradictory,
/// constraint on something already fixed.
const RESERVED_FILTER_FIELDS: &[&str] = &["gts_type_id", "window_start", "window_end"];

/// `true` when `name` names a [`RESERVED_FILTER_FIELDS`] entry, ignoring
/// ASCII case.
///
/// Case-insensitive to match [`require_bounded_time_window`]'s own
/// `created_at` identifier match (`name.eq_ignore_ascii_case(...)`) — the
/// identical kind of AST-identifier comparison — rather than a bare
/// `contains`. Not exploitable today (`window_start` / `window_end` are not
/// yet filterable-schema fields, and `gts_type_id` case-varied would
/// dead-end as `toolkit_odata`'s own case-insensitive `UnknownField`
/// downstream), but `window_start` / `window_end` become real filterable
/// fields in the record-model slice after this plan, at which point a
/// case-varied spelling would otherwise resolve as a legitimate field
/// instead of hitting this reservation.
fn is_reserved_filter_field(name: &str) -> bool {
    RESERVED_FILTER_FIELDS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Rejects a `$filter` naming a [`RESERVED_FILTER_FIELDS`] identifier,
/// wherever in the AST it appears.
///
/// Walks the **whole** tree rather than only top-level conjuncts: a
/// reserved identifier nested under an `or` (or a `not`, or an `in` list)
/// is just as much a constraint on the reserved field as one at the top
/// level, so a top-level-only check would let it through.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] naming the offending
/// field.
pub(crate) fn reject_reserved_filter_fields(filter: &ast::Expr) -> Result<(), UsageCollectorError> {
    match filter {
        ast::Expr::Identifier(name) if is_reserved_filter_field(name) => {
            Err(UsageCollectorError::reserved_filter_field(name))
        }
        ast::Expr::Identifier(_) | ast::Expr::Value(_) => Ok(()),
        ast::Expr::Not(inner) => reject_reserved_filter_fields(inner),
        ast::Expr::And(left, right) | ast::Expr::Or(left, right) => {
            reject_reserved_filter_fields(left)?;
            reject_reserved_filter_fields(right)
        }
        ast::Expr::Compare(left, _op, right) => {
            reject_reserved_filter_fields(left)?;
            reject_reserved_filter_fields(right)
        }
        ast::Expr::In(left, items) => {
            reject_reserved_filter_fields(left)?;
            items.iter().try_for_each(reject_reserved_filter_fields)
        }
        ast::Expr::Function(_name, args) => args.iter().try_for_each(reject_reserved_filter_fields),
    }
}

/// Checks every `group_by` dimension is either a fixed field (no
/// declaration needed) or a metadata property `declared_keys` actually
/// declares.
///
/// `declared_keys` is read from the resolved declaration fresh for this
/// request (see [`crate::domain::type_resolver::CompiledMetadataSchema::declared_keys`]),
/// never cached independently of it — so a property declared a moment ago
/// is usable on the very next call, per Spec §3.11. `gts_type_id` is the
/// queried meter, carried onto the error for operator-log parity with
/// [`UsageCollectorError::unknown_metadata_key`].
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] naming the first
/// undeclared metadata dimension.
pub(crate) fn require_dimensions_declared(
    dimensions: &[AggregationDimension],
    declared_keys: &BTreeSet<String>,
    gts_type_id: &MeterTypeId,
) -> Result<(), UsageCollectorError> {
    for dim in dimensions {
        if let AggregationDimension::Metadata(key) = dim
            && !declared_keys.contains(key.as_str())
        {
            return Err(UsageCollectorError::undeclared_metadata_dimension(
                gts_type_id,
                key.as_str(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "query_tests.rs"]
mod query_tests;
