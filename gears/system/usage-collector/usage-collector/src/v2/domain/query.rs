//! Query read-path helpers shared by both read paths.
//!
//! Holds the helpers `list_usage_records` AND `query_aggregated_usage_records`
//! both call, kept out of `service.rs` so it stays focused on orchestration.
//! The raw path's own cursor/keyset machinery lives in
//! [`crate::v2::domain::list_query`]; the aggregate path's `group_by`
//! declaredness check lives in [`crate::v2::domain::aggregate_query`].
//!
//! **The surface gate.** The aggregate path runs every check here plus
//! [`crate::v2::domain::aggregate_query::require_dimensions_declared`]; the raw
//! path runs every check here only, having no `group_by` to check. The
//! admissible surface is the published fixed fields plus the queried meter's
//! declared metadata keys, recomputed per request from the resolved
//! declaration and never cached independently of it, so a property declared
//! a moment ago is usable on the very next call.
//!
//! Each check lives here rather than in the REST handler, so an in-process
//! caller is gated too.

use std::collections::BTreeSet;

use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, ast};
use toolkit_security::AccessScope;
use usage_collector_sdk::v2::{
    EntryType, MetadataFilter, MeterTypeId, PUBLISHED_FILTER_FIELDS, RecordOrigin,
    UsageCollectorError, WINDOW_END_FIELD, WINDOW_START_FIELD,
};

use crate::v2::domain::authz;
use crate::v2::domain::ports::metrics::{QueryKind, UsageCollectorMetrics};

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
/// parameters that no [`ODataQuery`] carries as a predicate, so composition
/// cannot narrow, widen, or drop either of them.
///
/// Composition is intersection-only (no widening). PDP constraint shapes
/// outside the supported set (tree predicates, unknown properties, value-type
/// mismatches) bubble up as fail-closed
/// [`AuthorizationDenied`](crate::v2::domain::DomainError::AuthorizationDenied) from
/// [`authz::scope_to_odata_filter`].
// @cpt-algo:cpt-cf-usage-collector-algo-read-scope-composition:p1
// @cpt-dod:cpt-cf-usage-collector-dod-read-scope-composition:p1
pub(crate) fn compose_query_with_scope(
    user_query: &ODataQuery,
    scope: &AccessScope,
) -> Result<(ODataQuery, ast::Expr), UsageCollectorError> {
    // @cpt-begin:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-readcomp-base
    // @cpt-begin:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-constraint-composition-iterate
    // `scope_to_odata_filter` always yields a narrowing predicate or fails
    // closed: an unconstrained / empty-constraint / deny-all scope is denied,
    // never passed through as "no row narrowing".
    let scope_expr = authz::scope_to_odata_filter(scope).map_err(UsageCollectorError::from)?;
    // @cpt-end:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-constraint-composition-iterate
    // @cpt-end:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-readcomp-base

    // @cpt-begin:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-readcomp-conjoin
    let composed_filter: ast::Expr = match user_query.filter().cloned() {
        Some(user_expr) => user_expr.and(scope_expr.clone()),
        None => scope_expr.clone(),
    };
    // @cpt-end:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-readcomp-conjoin

    let mut composed = user_query.clone();
    // Preserve the caller's `filter_hash` (kept by the clone); do NOT re-hash
    // the AND-merged filter. The cursor's `f` exists to detect the *caller*
    // changing their query between paginated requests, so the server-injected
    // PDP scope must stay out: re-hashing to `hash(user AND scope)` would embed
    // a value the follow-up request's own recomputation can never reproduce,
    // breaking pagination with a spurious `FILTER_MISMATCH` 400 the moment PDP
    // returns any row scope.
    composed.filter = Some(Box::new(composed_filter));
    // @cpt-begin:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-readcomp-return
    // Handed back so the dispatch can name it as its own SPI argument, rather
    // than recomputed there: two derivations of one predicate can drift, and
    // the SPI's guarantee is that `scope` is *the* conjunct folded in here.
    Ok((composed, scope_expr))
    // @cpt-end:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-readcomp-return
}

/// Field names a caller may never name in a `$filter`.
///
/// `gts_type_id` travels as a typed parameter and the covered period as a
/// typed [`TimeRange`](usage_collector_sdk::v2::TimeRange), so a predicate over
/// any of the three would express a second, possibly contradictory,
/// constraint on something already fixed.
///
/// Both covered-period bounds *are* filterable-schema fields
/// ([`usage_collector_sdk::v2::UsageRecordFilterField`]) — that schema doubles
/// as the plugin's field-to-column mapping and the `$orderby` vocabulary,
/// and `window_end` has to resolve to a column for the canonical keyset to
/// mean anything. This guard, not their absence from the schema, is the
/// whole reason a `$filter` cannot reach them, which is what keeps the
/// covered-period-end predicate the only way a range can be expressed.
// @cpt-algo:cpt-cf-usage-collector-algo-period-end-selection:p1
// @cpt-dod:cpt-cf-usage-collector-dod-period-end-selection:p1
const RESERVED_FILTER_FIELDS: &[&str] = &[
    // `gts_type_id` has no SDK constant: it is not a filterable-schema
    // field at all, so there is nothing to point at.
    "gts_type_id",
    // The bounds come from the SDK constants rather than being respelled: a
    // typo here would silently un-reserve a field, the guard simply stopping
    // to match.
    WINDOW_START_FIELD,
    WINDOW_END_FIELD,
];

/// `true` when `name` names a [`RESERVED_FILTER_FIELDS`] entry, ignoring
/// ASCII case.
///
/// Case-insensitive rather than a bare `contains` because **this guard has
/// to match its own downstream resolver**: for a `$filter` identifier that is
/// `toolkit_odata::filter::FilterField::from_name`, which compares with
/// `eq_ignore_ascii_case`. So `WINDOW_END` in a `$filter` folds to the
/// `WindowEnd` variant downstream and reaches a plugin as a real predicate
/// on the `window_end` column; an exact comparison here would let a
/// case-varied spelling of a reserved field walk straight past a reservation
/// whose whole purpose is to be un-evadable.
///
/// The `$orderby` guards next door match **exactly**, which is the same rule
/// against a different resolver: an `$orderby` key reaches the plugin's
/// `record_column` as the caller's own string, and that is an exact `match`.
fn is_reserved_filter_field(name: &str) -> bool {
    RESERVED_FILTER_FIELDS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// `true` when `name` names a [`PUBLISHED_FILTER_FIELDS`] entry, ignoring
/// ASCII case.
///
/// An **allowlist** rather than a second denylist, whose alternative needs an
/// entry per field the generated schema happens to declare — a list that rots
/// the moment the schema grows. The set itself lives in the SDK
/// ([`PUBLISHED_FILTER_FIELDS`]) because
/// [`UsageCollectorError::unpublished_filter_field`] renders the whole
/// admissible set into its `detail`, so the refusal prose cannot drift from
/// the constant this guard tests against.
///
/// Case-insensitive for [`is_reserved_filter_field`]'s reason, which here cuts
/// the other way: on an allowlist an exact comparison *refuses* more, so it
/// would 400 a predicate the rest of the stack handles correctly end to end —
/// `TENANT_ID` folds to the `TenantId` variant and `translate_filter` resolves
/// it off the enum's canonical name, never the caller's string.
fn is_published_filter_field(name: &str) -> bool {
    PUBLISHED_FILTER_FIELDS
        .iter()
        .any(|published| name.eq_ignore_ascii_case(published))
}

/// Rejects a `$filter` naming any field outside [`PUBLISHED_FILTER_FIELDS`],
/// wherever in the AST it appears, with the reason that field earns.
///
/// Two reasons, deliberately, because the two cases send a caller to
/// different places:
///
/// * a [`RESERVED_FILTER_FIELDS`] entry travels as a typed parameter, so
///   the refusal says so and the caller moves the predicate there;
/// * anything else off [`PUBLISHED_FILTER_FIELDS`] is simply not on the
///   surface, and telling that caller about a typed parameter would send
///   them looking for one that does not exist. `id` is the live case: the
///   generated schema declares it filterable (it is the canonical
///   keyset's final tiebreaker, and has to resolve to a column for that),
///   but the contract does not publish it.
///
/// Reserved is checked **before** published, so `window_end` — which is
/// off the published eight *and* reserved — gets the reserved reason, the
/// more useful one.
///
/// Walks the **whole** tree rather than top-level conjuncts: a field nested
/// under an `or` (or a `not`, or an `in` list) is just as much a constraint on
/// it as one at the top level. Matching is case-insensitive on both arms, per
/// [`is_reserved_filter_field`].
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] naming the offending field.
// @cpt-algo:cpt-cf-usage-collector-algo-query-field-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-query-field-validation:p1
pub(crate) fn reject_unpublished_filter_fields(
    filter: &ast::Expr,
) -> Result<(), UsageCollectorError> {
    match filter {
        ast::Expr::Identifier(name) if is_reserved_filter_field(name) => {
            Err(UsageCollectorError::reserved_filter_field(name))
        }
        ast::Expr::Identifier(name) if !is_published_filter_field(name) => {
            Err(UsageCollectorError::unpublished_filter_field(name))
        }
        ast::Expr::Identifier(_) | ast::Expr::Value(_) => Ok(()),
        ast::Expr::Not(inner) => reject_unpublished_filter_fields(inner),
        ast::Expr::And(left, right) | ast::Expr::Or(left, right) => {
            reject_unpublished_filter_fields(left)?;
            reject_unpublished_filter_fields(right)
        }
        ast::Expr::Compare(left, _op, right) => {
            reject_unpublished_filter_fields(left)?;
            reject_unpublished_filter_fields(right)
        }
        ast::Expr::In(left, items) => {
            reject_unpublished_filter_fields(left)?;
            items.iter().try_for_each(reject_unpublished_filter_fields)
        }
        ast::Expr::Function(_name, args) => {
            args.iter().try_for_each(reject_unpublished_filter_fields)
        }
    }
}

/// The `$filter` fields whose values come from a closed set, and that set.
///
/// Derived from the SDK's own types rather than respelled here: a host-side
/// copy of a label list is the drift `RESERVED_FILTER_FIELDS`' comment
/// warns about, and these two sets are the plugin's SQL enum and CHECK
/// constraint respectively.
fn closed_label_set(field: &str) -> Option<&'static [&'static str]> {
    if field.eq_ignore_ascii_case("entry_type") {
        Some(EntryType::wire_labels())
    } else if field.eq_ignore_ascii_case("origin") {
        Some(RecordOrigin::wire_labels())
    } else {
        None
    }
}

/// The caller-supplied string literal an `ast::Expr` carries, or `None` for
/// anything else — an identifier, a non-string value, a comparison operand
/// that is itself a nested expression. Closed-label columns are string-typed
/// on the wire (`EntryType` / `RecordOrigin` both `#[serde(rename_all =
/// "lowercase")]` onto a string), so a non-string literal compared against
/// one is a shape mismatch this guard has no opinion on, not a label check.
fn literal_str(expr: &ast::Expr) -> Option<&str> {
    match expr {
        ast::Expr::Value(ast::Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// Checks one `(field_side, value_side)` argument order of a `Compare` for a
/// closed-label field compared against an off-label literal.
///
/// Silently `Ok` whenever `field_side` is not an `Identifier` naming a
/// closed-label field, or `value_side` is not a string literal: those are
/// shapes this guard has no closed set to check against, not admissible
/// literals it approved. `OData` admits `'Record' eq entry_type` as well as
/// `entry_type eq 'Record'`, so [`reject_off_label_literals`] calls this
/// once per order on every `Compare`.
///
/// **The two sides compare differently, and the asymmetry is deliberate.**
/// The field *name* is matched case-insensitively ([`closed_label_set`]'s
/// `eq_ignore_ascii_case`), so `$filter=Entry_Type eq 'record'` cannot
/// side-step the guard by respelling the column. The *value* is matched
/// exactly (`labels.contains`), because the backend compares it exactly —
/// `entry_type`'s Postgres enum and `origin`'s `CHECK` constraint are both
/// case-sensitive, so `'Record'` is off-label in fact and not merely in
/// spelling. Folding the value here would admit a literal the backend then
/// refuses or mismatches, which is the one outcome worse than refusing it up
/// front.
fn check_closed_pair(
    field_side: &ast::Expr,
    value_side: &ast::Expr,
) -> Result<(), UsageCollectorError> {
    let ast::Expr::Identifier(name) = field_side else {
        return Ok(());
    };
    let Some(labels) = closed_label_set(name) else {
        return Ok(());
    };
    let Some(value) = literal_str(value_side) else {
        return Ok(());
    };
    if labels.contains(&value) {
        Ok(())
    } else {
        Err(UsageCollectorError::inadmissible_filter_literal(
            name, value, labels,
        ))
    }
}

/// Rejects a `$filter` comparing a closed-label field against a value
/// outside its set, before dispatch.
///
/// Each closed-label column answers wrongly in its own way.
/// `entry_type` is a written SQL enum and the plugin casts the **literal**
/// per its DESIGN §3.7, so an off-label value raises `PostgreSQL` `22P02` and
/// reaches the caller as a server-class error — caller input producing a
/// 5xx. `origin` is `text` with a CHECK constraint, so an
/// off-label value answers an empty page, which is a silently wrong answer
/// to a malformed request. Both become a 400 naming the value and the
/// admissible labels.
///
/// Gear-side rather than in the translate layer: `UsageCollectorPluginError`
/// carries no invalid-argument variant, so a translate-layer guard returns
/// `Internal` and a 500 regardless, and a gear-side guard covers the
/// in-process surface as well as REST.
///
/// Walks the whole tree and both comparison shapes. An `in` list is a list
/// of literals and each is checked, because a guard inspecting only
/// `Compare` would serve the request from the admissible half of the list.
///
/// **Operator-blind, deliberately.** [`check_closed_pair`] never reads
/// `_op`, so `entry_type ne 'Record'` and `not (entry_type eq 'Record')` are
/// refused exactly like `entry_type eq 'Record'` — a predicate *naming* a
/// value outside the closed set is a confused request whichever operator
/// compares against it. A known, accepted narrowing: `origin ne 'imported'`
/// used to return a semantically correct full page and now draws a 400.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] naming the offending value and
/// the labels the field admits.
pub(crate) fn reject_off_label_literals(filter: &ast::Expr) -> Result<(), UsageCollectorError> {
    match filter {
        ast::Expr::Compare(left, _op, right) => {
            check_closed_pair(left, right)?;
            check_closed_pair(right, left)
        }
        ast::Expr::In(left, items) => {
            if let ast::Expr::Identifier(name) = left.as_ref()
                && let Some(labels) = closed_label_set(name)
            {
                for item in items {
                    if let Some(value) = literal_str(item)
                        && !labels.contains(&value)
                    {
                        return Err(UsageCollectorError::inadmissible_filter_literal(
                            name, value, labels,
                        ));
                    }
                }
            }
            Ok(())
        }
        ast::Expr::Not(inner) => reject_off_label_literals(inner),
        ast::Expr::And(left, right) | ast::Expr::Or(left, right) => {
            reject_off_label_literals(left)?;
            reject_off_label_literals(right)
        }
        ast::Expr::Function(_name, args) => args.iter().try_for_each(reject_off_label_literals),
        ast::Expr::Identifier(_) | ast::Expr::Value(_) => Ok(()),
    }
}

/// The most distinct `metadata_filter` predicates one read admits.
///
/// Published in the `POST /records/aggregate` operation description as prose
/// rather than `maxItems`, so this constant is the cap's only enforcement. It
/// lives here rather than in the REST handler because this module is the one
/// point every surface passes through — REST, the in-process client and a
/// direct `Service` call alike.
pub(crate) const MAX_METADATA_FILTERS: usize = 16;

/// The most values one `metadata_filter` predicate admits.
///
/// Published as `maxItems: 32` on `MetadataFilter.values`, which is no longer
/// reachable by `$ref` from either wire path, so — like
/// [`MAX_METADATA_FILTERS`] — this constant is the cap's only enforcement.
pub(crate) const MAX_METADATA_FILTER_VALUES: usize = 32;

/// Requires a `metadata_filter` to be within the published caps.
///
/// Two bounds, and they are on the wire contract rather than on one path's
/// machinery, so this runs on **both** read paths. On the raw path it has a
/// second job: the predicate count and value count are what keep a minted
/// cursor inside the published `maxLength: 4096`, since the fingerprint the
/// token carries is computed over them.
///
/// The wire field names are the REST handler's — `metadata` for the count
/// and `metadata.<key>` for the values — so enforcing here does not move the
/// error a caller sees.
///
/// **Entries, not distinct keys.** `metadata_filter.len()` counts slice
/// entries, and `&[MetadataFilter]` can carry the same key twice, so a caller
/// repeating a key hits the cap one entry sooner than a distinct-key count
/// would. That is what bounds the fingerprint and the SQL a plugin builds.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] naming the cap that was
/// exceeded and by how much.
pub(crate) fn require_metadata_filter_within_caps(
    metadata_filter: &[MetadataFilter],
) -> Result<(), UsageCollectorError> {
    if metadata_filter.len() > MAX_METADATA_FILTERS {
        return Err(UsageCollectorError::too_many_metadata_filters(
            metadata_filter.len(),
            MAX_METADATA_FILTERS,
        ));
    }
    if let Some(filter) = metadata_filter
        .iter()
        .find(|f| f.values().len() > MAX_METADATA_FILTER_VALUES)
    {
        return Err(UsageCollectorError::too_many_metadata_filter_values(
            filter.key().as_str(),
            filter.values().len(),
            MAX_METADATA_FILTER_VALUES,
        ));
    }
    Ok(())
}

/// Checks every `metadata_filter` entry names a metadata key
/// `declared_keys` actually declares.
///
/// `metadata_filter` is the dynamic-key side channel that exists precisely
/// because the `toolkit-odata` grammar cannot express a filter over a JSON
/// map key — it never flows through `$filter` at all, so
/// [`reject_unpublished_filter_fields`] and this check gate two disjoint
/// surfaces. Without this check an undeclared key would silently narrow
/// the result set to nothing (a plugin equality-filters on a column /
/// property that no row carries) rather than fail with an actionable 400.
///
/// `declared_keys` is read from the resolved declaration fresh for this
/// request, never cached independently of it — so a property declared a
/// moment ago is usable on the very next call.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] (via
/// [`UsageCollectorError::unknown_metadata_key`]) naming the first
/// undeclared key.
// @cpt-algo:cpt-cf-usage-collector-algo-query-field-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-query-field-validation:p1
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

/// RAII guard for the `uc_query_inflight{query_kind}` gauge: increments on
/// [`Self::enter`] (called once authorization composes) and decrements on
/// `Drop` — so the gauge is decremented on *every* exit that followed the
/// increment (including `?` early returns) and never drained without a prior
/// bump.
#[domain_model]
pub(crate) struct QueryInflightGuard<'a> {
    metrics: &'a dyn UsageCollectorMetrics,
    kind: QueryKind,
}

impl<'a> QueryInflightGuard<'a> {
    // @cpt-begin:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-inflight-increment
    // @cpt-begin:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-inflight-increment
    pub(crate) fn enter(metrics: &'a dyn UsageCollectorMetrics, kind: QueryKind) -> Self {
        metrics.query_inflight_inc(kind);
        Self { metrics, kind }
    }
    // @cpt-end:cpt-cf-usage-collector-flow-query-aggregated-usage:p1:inst-aggregated-inflight-increment
    // @cpt-end:cpt-cf-usage-collector-flow-query-raw-ledger-page:p1:inst-raw-inflight-increment
}

impl Drop for QueryInflightGuard<'_> {
    fn drop(&mut self) {
        self.metrics.query_inflight_dec(self.kind);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "query_tests.rs"]
mod query_tests;
