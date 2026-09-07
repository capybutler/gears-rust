//! Query read-path helpers for the usage-collector domain service.
//!
//! Holds the helpers that serve only the `list_usage_records` /
//! `query_aggregated_usage_records` read paths, kept out of `service.rs`
//! so it stays focused on orchestration:
//!
//! * `compose_query_with_scope` — AND-merges the PDP-returned
//!   [`AccessScope`] into the caller's `$filter`.
//! * `reject_reserved_filter_fields` — rejects a `$filter` naming a field
//!   reserved to a typed parameter (`gts_type_id`, the covered-period
//!   bounds), wherever in the AST it appears.
//! * `require_dimensions_declared` — checks a `group_by` list only names
//!   the fixed dimensions or a metadata key the queried meter's resolved
//!   declaration actually declares.
//! * `require_metadata_filter_keys_declared` — the same check for
//!   `metadata_filter`, the dynamic-key side channel that exists precisely
//!   because the `toolkit-odata` grammar cannot express filters over JSON
//!   map keys, so it never flows through `$filter` at all.
//! * `ensure_admissible_keyset_order` — the raw path's keyset floor: it
//!   guarantees every dispatch carries the non-empty, uniform-direction,
//!   never-null order the Plugin SPI promises, whichever surface the call
//!   came in on.
//!
//! Per Spec §3.11, the admissible filter and grouping surface is the fixed
//! fields (via `$filter`, gated by [`reject_reserved_filter_fields`]) plus
//! the queried meter's declared metadata keys (via `group_by` and
//! `metadata_filter`, gated by [`require_dimensions_declared`] and
//! [`require_metadata_filter_keys_declared`] respectively) — recomputed
//! per request from the resolved declaration, never cached independently
//! of it, so a property declared a moment ago is usable on the very next
//! call.

use std::collections::BTreeSet;

use toolkit_odata::{ODataQuery, SortDir, ast};
use toolkit_security::AccessScope;
use usage_collector_sdk::{
    AggregationDimension, MetadataFilter, MeterTypeId, UsageCollectorError, WINDOW_END_FIELD,
    is_keyset_safe_record_field,
};

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
/// the composed filter, which is what makes an empty `$filter` a complete
/// request. The order / limit / cursor / select projections on
/// [`ODataQuery`] flow through verbatim — the composition only touches the
/// `$filter` AST — and `gts_type_id` and the read range are typed
/// parameters that never enter an [`ODataQuery`] at all, so composition
/// cannot narrow, widen, or drop either of them.
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

/// Field names a caller may never name in a `$filter`.
///
/// `gts_type_id` travels as a typed parameter and the covered period as a
/// typed [`TimeRange`](usage_collector_sdk::TimeRange), so a predicate over
/// any of the three would express a second, possibly contradictory,
/// constraint on something already fixed.
///
/// Both covered-period bounds *are* filterable-schema fields
/// ([`usage_collector_sdk::UsageRecordFilterField`]) — that schema doubles
/// as the plugin's field-to-column mapping and the `$orderby` vocabulary,
/// and `window_end` has to resolve to a column for the canonical
/// `(window_end, id)` keyset to mean anything. This guard, not their
/// absence from the schema, is the whole reason a `$filter` cannot reach
/// them.
const RESERVED_FILTER_FIELDS: &[&str] = &["gts_type_id", "window_start", "window_end"];

/// `true` when `name` names a [`RESERVED_FILTER_FIELDS`] entry, ignoring
/// ASCII case.
///
/// Case-insensitive rather than a bare `contains` because the reservation
/// has to be un-evadable: its whole purpose is that no `$filter` naming a
/// reserved field reaches a plugin, and a case-varied spelling would
/// otherwise walk straight past it. That reason is local to this check
/// rather than a gear-wide convention — the `$orderby` guards next door
/// ([`usage_collector_sdk::is_keyset_safe_record_field`] and
/// `toolkit_odata::ODataOrderBy::ensure_tiebreaker`) both match exactly.
///
/// The case-insensitivity is load-bearing rather than theoretical:
/// `window_start` / `window_end` are filterable-schema fields, so a
/// case-varied spelling of one resolves as a legitimate field and would
/// reach a plugin as a real predicate if this comparison were exact.
/// (A case-varied `gts_type_id` would additionally dead-end downstream as
/// `toolkit_odata`'s own case-insensitive `UnknownField`, since that name
/// is off the schema entirely — but the bounds have no such second net.)
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

/// The canonical unique keyset suffix every raw-list order ends in.
///
/// `window_end` is the primary time key — the same column the read range
/// selects on (`cpt-cf-usage-collector-adr-window-end-selection`), so one
/// index serves both the selection and the page order — and `id` is the
/// globally-unique final tiebreaker that stops a page boundary landing
/// inside a run of rows sharing a `window_end`.
const CANONICAL_KEYSET_SUFFIX: &[&str] = &[WINDOW_END_FIELD, "id"];

/// Guarantees `query.order` is the keyset the Plugin SPI promises: a
/// non-empty, uniform-direction, never-null order ending in
/// [`CANONICAL_KEYSET_SUFFIX`].
///
/// This is the raw path's keyset **floor**, and it lives here rather than
/// in the REST handler because DESIGN §3.1 allocates order admissibility
/// to the Query Gateway, which §3.2 exists to keep uniform across the SDK
/// and REST. An in-process caller reaches
/// [`Service::list_usage_records`](crate::domain::Service::list_usage_records)
/// with an [`ODataQuery`] of their own construction — an empty order, most
/// of the time — so a normalization that only ran at the REST edge left
/// the SPI's order slot unpopulated on exactly the surface no REST test
/// can reach.
///
/// Two properties have to hold before the suffix can be appended at all,
/// and neither can be repaired by appending:
///
/// * **One direction.** The plugin's continuation is a row-value tuple
///   comparison (`(c1, c2, …) > ($…)`), which has no meaning across mixed
///   directions.
/// * **No nullable key.** A tuple whose leading column is NULL compares as
///   NULL in SQL's three-valued logic, so every NULL-keyed row silently
///   drops out of the page and a page ending on one cannot encode a
///   cursor at all. [`is_keyset_safe_record_field`] is the fail-closed
///   classification, so an unrecognised name is refused on the same
///   branch.
///
/// The suffix is then appended in the order's own direction (`Asc` for an
/// empty order) via [`toolkit_odata::ODataOrderBy::ensure_tiebreaker`],
/// which skips a field the order already names — so this is idempotent,
/// and applying it after REST has validated an order is a no-op rather
/// than a double append.
///
/// `prepare_list_query` calls this too, on the caller's `$orderby` before
/// any authorization or plugin work happens, so a wire request is refused
/// where its input is parsed and the `400` blames the parameter the caller
/// actually sent. That mirroring is the point of the idempotence: the same
/// rule, stated once, applied at the edge for the message and again here
/// for the guarantee.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] when the order mixes
/// sort directions, or names a key that is not a mandatory record
/// attribute.
pub(crate) fn ensure_admissible_keyset_order(
    query: &mut ODataQuery,
) -> Result<(), UsageCollectorError> {
    let keys = &query.order.0;
    if let Some(first) = keys.first()
        && keys.iter().any(|key| key.dir != first.dir)
    {
        return Err(UsageCollectorError::mixed_direction_order());
    }
    if let Some(bad) = keys
        .iter()
        .find(|key| !is_keyset_safe_record_field(&key.field))
    {
        return Err(UsageCollectorError::inadmissible_order_key(&bad.field));
    }

    let dir = keys.last().map_or(SortDir::Asc, |key| key.dir);
    let mut order = std::mem::take(&mut query.order);
    for &field in CANONICAL_KEYSET_SUFFIX {
        order = order.ensure_tiebreaker(field, dir);
    }
    query.order = order;
    Ok(())
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

/// Checks every `metadata_filter` entry names a metadata key
/// `declared_keys` actually declares.
///
/// `metadata_filter` is the dynamic-key side channel that exists precisely
/// because the `toolkit-odata` grammar cannot express a filter over a JSON
/// map key — it never flows through `$filter` at all, so
/// [`reject_reserved_filter_fields`] and this check gate two disjoint
/// surfaces. Without this check an undeclared key would silently narrow
/// the result set to nothing (a plugin equality-filters on a column /
/// property that no row carries) rather than fail with an actionable 400.
///
/// `declared_keys` is read from the resolved declaration fresh for this
/// request, never cached independently of it — so a property declared a
/// moment ago is usable on the very next call, per Spec §3.11.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] (via
/// [`UsageCollectorError::unknown_metadata_key`]) naming the first
/// undeclared key.
pub(crate) fn require_metadata_filter_keys_declared(
    metadata_filter: &[MetadataFilter],
    declared_keys: &BTreeSet<String>,
    gts_type_id: &MeterTypeId,
) -> Result<(), UsageCollectorError> {
    for filter in metadata_filter {
        let key = filter.key().as_str();
        if !declared_keys.contains(key) {
            return Err(UsageCollectorError::unknown_metadata_key(gts_type_id, key));
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "query_tests.rs"]
mod query_tests;
