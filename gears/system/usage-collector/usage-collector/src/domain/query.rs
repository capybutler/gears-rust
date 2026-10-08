//! Query read-path helpers shared by both read paths.
//!
//! Holds the helpers `list_usage_records` AND `query_aggregated_usage_records`
//! both call, kept out of `service.rs` so it stays focused on orchestration.
//! The raw path's own cursor/keyset machinery lives in
//! [`crate::domain::list_query`]; the aggregate path's `group_by`
//! declaredness check lives in [`crate::domain::aggregate_query`]. Every
//! `pub(crate)` item here is listed, in the three groups the read paths call
//! them in.
//!
//! **Scope composition**
//!
//! * `compose_query_with_scope` — AND-merges the PDP-returned
//!   [`AccessScope`] into the caller's `$filter`.
//!
//! **The Spec §3.11 surface gate.** The aggregate path runs every check below
//! plus [`crate::domain::aggregate_query::require_dimensions_declared`]; the
//! raw path runs every check below only, having no `group_by` to check. The
//! admissible surface is the published fixed fields plus the queried meter's
//! declared metadata keys, recomputed per request from the resolved
//! declaration and never cached independently of it, so a property declared
//! a moment ago is usable on the very next call.
//!
//! * `reject_unpublished_filter_fields` — rejects a `$filter` naming any field
//!   outside [`PUBLISHED_FILTER_FIELDS`], wherever in the AST it appears: a field
//!   reserved to a typed parameter (`gts_type_id`, the covered-period bounds)
//!   with its own reason, anything else off the set with a second.
//! * `reject_off_label_literals` — rejects a `$filter` comparing a closed-label
//!   field (`entry_type`, `origin`) against a literal outside its label set,
//!   which the backend would otherwise answer wrongly. Field membership and
//!   literal admissibility are two separate gates.
//! * `require_metadata_filter_keys_declared` — the declaredness check for
//!   `metadata_filter`, the dynamic-key side channel that exists because the
//!   `toolkit-odata` grammar cannot express filters over JSON map keys, so it
//!   never flows through `$filter` at all.
//! * `require_metadata_filter_within_caps` — caps `metadata_filter` at
//!   `MAX_METADATA_FILTERS` predicates of `MAX_METADATA_FILTER_VALUES` values
//!   each, here rather than in the REST handler, so an in-process caller is
//!   capped too.
//!
//! **Shared instrumentation**
//!
//! * `QueryInflightGuard` — RAII guard for the `uc_query_inflight{query_kind}`
//!   gauge: increments on entry and decrements on `Drop`, so both the raw and
//!   the aggregate path share one bump-on-every-exit implementation rather
//!   than each keeping its own.

use std::collections::BTreeSet;

use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, ast};
use toolkit_security::AccessScope;
use usage_collector_sdk::{
    EntryType, MetadataFilter, MeterTypeId, PUBLISHED_FILTER_FIELDS, RecordOrigin,
    UsageCollectorError, WINDOW_END_FIELD, WINDOW_START_FIELD,
};

use crate::domain::authz;
use crate::domain::ports::metrics::{QueryKind, UsageCollectorMetrics};

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
/// cannot narrow, widen, or drop either of them. (The read path does fold
/// both, and the metadata filter, into `filter_hash` afterwards via
/// [`crate::domain::list_query::read_fingerprint`], but that is an opaque
/// pagination fingerprint and not a row constraint.)
///
/// Per `cpt-cf-usage-collector-algo-read-scope-composition`:
/// composition is intersection-only (no widening). PDP constraint
/// shapes outside the supported set (tree predicates, unknown
/// properties, value-type mismatches) bubble up as fail-closed
/// [`AuthorizationDenied`](crate::domain::DomainError::AuthorizationDenied) from
/// [`authz::scope_to_odata_filter`].
// @cpt-algo:cpt-cf-usage-collector-algo-read-scope-composition:p1
// @cpt-dod:cpt-cf-usage-collector-dod-read-scope-composition:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn compose_query_with_scope(
    user_query: &ODataQuery,
    scope: &AccessScope,
) -> Result<ODataQuery, UsageCollectorError> {
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
        Some(user_expr) => user_expr.and(scope_expr),
        None => scope_expr,
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
    //
    // The dispatched query carries neither value — the service computes
    // `read_fingerprint` from the caller's `$filter` and the typed parameters,
    // holds it locally, and strips `composed.filter_hash` to `None` before the
    // plugin sees it. The preservation still has to hold, because a re-hash
    // here would be the composed filter reaching the fingerprint by the back
    // door.
    composed.filter = Some(Box::new(composed_filter));
    // @cpt-begin:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-readcomp-return
    Ok(composed)
    // @cpt-end:cpt-cf-usage-collector-algo-read-scope-composition:p1:inst-readcomp-return
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
/// and `window_end` has to resolve to a column for the canonical keyset to
/// mean anything. This guard, not their absence from the schema, is the
/// whole reason a `$filter` cannot reach them.
///
/// Traceability: the gear's half of
/// `cpt-cf-usage-collector-algo-period-end-selection` /
/// `cpt-cf-usage-collector-dod-period-end-selection` — reserving
/// `window_start` and `window_end` is what keeps the covered-period-end
/// predicate the *only* way a range can be expressed, structurally
/// precluding a caller-supplied `window_start`-based alternative. The
/// comparison itself (`from <= window_end < to`) executes in the bound
/// storage plugin, per the Plugin SPI's own doc on
/// `UsageCollectorPluginV1::list_usage_records` /
/// `query_aggregated_usage_records`.
// @cpt-algo:cpt-cf-usage-collector-algo-period-end-selection:p1
// @cpt-dod:cpt-cf-usage-collector-dod-period-end-selection:p1
const RESERVED_FILTER_FIELDS: &[&str] = &[
    // `gts_type_id` has no SDK constant: it is not a filterable-schema
    // field at all, so there is nothing to point at.
    "gts_type_id",
    // The bounds come from the SDK constants rather than being respelled.
    // This is the one host-side list where a typo silently un-reserves a
    // field — the guard would simply stop matching — and it is exactly the
    // drift the constants were introduced to prevent.
    WINDOW_START_FIELD,
    WINDOW_END_FIELD,
];

/// `true` when `name` names a [`RESERVED_FILTER_FIELDS`] entry, ignoring
/// ASCII case.
///
/// Case-insensitive rather than a bare `contains` because **this guard has
/// to match its own downstream resolver**, and for a `$filter` identifier
/// that resolver is `toolkit_odata::filter::FilterField::from_name`
/// (`libs/toolkit-odata/src/filter.rs`), whose default impl — the one the
/// `ODataFilterable` derive leaves in place for
/// [`usage_collector_sdk::UsageRecordFilterField`] — compares with
/// `eq_ignore_ascii_case`. So `WINDOW_END` in a `$filter` folds to the
/// `WindowEnd` variant downstream and reaches a plugin as a real predicate
/// on the `window_end` column. An exact comparison here would therefore let
/// a case-varied spelling of a reserved field walk straight past a
/// reservation whose whole purpose is to be un-evadable.
///
/// The case-insensitivity is load-bearing rather than theoretical, and the
/// reserved three are not alike in this: `window_start` / `window_end` are
/// filterable-schema fields, so `from_name` resolves a case-varied spelling
/// of either and this guard is the only thing in front of it. A case-varied
/// `gts_type_id` would additionally dead-end at `from_name` itself as
/// `UnknownField`, since that name is off the schema entirely — the bounds
/// have no such second net.
///
/// The `$orderby` guards next door
/// ([`usage_collector_sdk::is_keyset_safe_record_field`] and
/// `toolkit_odata::ODataOrderBy::ensure_tiebreaker`) match **exactly**, and
/// that asymmetry is neither arbitrary nor a style choice: it is each side
/// agreeing with its own resolver. An `$orderby` key never passes through
/// `from_name` — the plugin's `render_order_by` hands the caller's own
/// string to `record_column`, an exact `match` — so there the exact
/// comparison is the agreeing one. See
/// [`reject_unpublished_filter_fields`].
fn is_reserved_filter_field(name: &str) -> bool {
    RESERVED_FILTER_FIELDS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// `true` when `name` names a [`PUBLISHED_FILTER_FIELDS`] entry, ignoring
/// ASCII case.
///
/// An **allowlist** rather than a second denylist, because that is what
/// `cpt-cf-usage-collector-dod-query-field-validation` asks for — "A filter
/// operand MUST be a member of the fixed filter field set" — and because
/// the alternative needs a denylist entry per field the generated schema
/// happens to declare, which is a list that rots the moment the schema
/// grows. The generated schema declares eleven: the published eight, the
/// two covered-period bounds, and `id`.
///
/// The set itself lives in the SDK rather than here
/// ([`PUBLISHED_FILTER_FIELDS`]) because the refusal
/// [`UsageCollectorError::unpublished_filter_field`] renders the whole
/// admissible set into its `detail`, and that prose is the actionable half
/// of that same definition of done — built from the constant this guard
/// tests against, it cannot drift from it.
///
/// Case-insensitive for [`is_reserved_filter_field`]'s reason, which is
/// **not** that an exact comparison would admit more. On an allowlist an
/// exact comparison refuses more, so evasion is not the hazard here; the
/// hazard is the opposite one. This guard decides admissibility for a name
/// that `toolkit_odata::filter::FilterField::from_name` will then resolve
/// case-insensitively, so an exact comparison here would 400 a predicate
/// the rest of the stack handles correctly end to end: `TENANT_ID` folds to
/// the `TenantId` variant, `translate_filter` resolves it as
/// `record_column(field.name())` — the canonical name off the enum, never
/// the caller's string — and the query runs. Refusing it would be a
/// gratuitous 400 on a working surface. The rule is the same one stated at
/// [`is_reserved_filter_field`]: each gear-side admissibility comparison
/// matches its own downstream resolver, and `$filter`'s is
/// `FilterField::from_name`.
fn is_published_filter_field(name: &str) -> bool {
    PUBLISHED_FILTER_FIELDS
        .iter()
        .any(|published| name.eq_ignore_ascii_case(published))
}

/// Rejects a `$filter` naming any field outside [`PUBLISHED_FILTER_FIELDS`],
/// wherever in the AST it appears, with the reason that field earns.
///
/// Two reasons, deliberately, because
/// `cpt-cf-usage-collector-dod-query-field-validation` requires an
/// actionable error and the two cases send a caller to different places:
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
/// **`$filter=id` is a behaviour change, not only a correctness decision.**
/// The `TimescaleDB` plugin's `record_column` maps `"id"` to the `id` column
/// (`query/translate.rs`), so an `id` predicate translated, ran, and returned a
/// correct page. It now draws a 400 — worth a release note, since every other
/// site frames `id`'s exclusion purely as a decision about the published
/// surface.
///
/// Reserved is checked **before** published, so `window_end` — which is
/// off the published eight *and* reserved — gets the reserved reason, the
/// more useful one.
///
/// Walks the **whole** tree rather than top-level conjuncts, for the
/// reason this check already carried when it only guarded
/// [`RESERVED_FILTER_FIELDS`]: a field nested under an `or` (or a `not`, or
/// an `in` list) is just as much a constraint on it as one at the top
/// level.
///
/// Matching is **case-insensitive on both arms**, so this guard agrees with
/// its own downstream resolver, `FilterField::from_name`, which is also
/// case-insensitive. The `$orderby` guards next door match **exactly**, which
/// is the same rule against a different resolver: an `$orderby` key reaches the
/// plugin's `record_column` as the caller's own string, and that is an exact
/// `match`. See [`is_reserved_filter_field`].
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] naming the offending field.
// @cpt-algo:cpt-cf-usage-collector-algo-query-field-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-query-field-validation:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
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
/// exactly (`labels.contains`), because the backend compares it exactly:
/// `entry_type`'s Postgres enum
/// (`CREATE TYPE usage_entry_type AS ENUM ('record', 'invalidation')`) and
/// `origin`'s `CHECK (origin IN ('live', 'backfill'))` are both
/// case-sensitive, so `'Record'` is off-label in fact and not merely in
/// spelling — it is precisely the `22P02` 500 and the silently-empty page
/// this guard exists to replace. Folding the value here would admit a
/// literal the backend then refuses or mismatches, which is the one
/// outcome worse than refusing it up front. This is the same
/// exact-match-with-a-stated-reason rule
/// [`crate::domain::aggregate_query::require_dimensions_declared`]'s uniqueness
/// pass argues, pointing the other way: there an exact comparison keeps a
/// case-differing *typo*
/// distinguishable from a repeat, here it keeps a case-differing literal
/// distinguishable from an admissible label.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
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
/// Gear-side rather than in the translate layer:
/// `UsageCollectorPluginError` carries no invalid-argument variant, so a
/// translate-layer guard returns `Internal` and a 500 regardless;
/// `cpt-cf-usage-collector-dod-query-field-validation` already requires
/// caller-supplied values be validated "before dispatching anything to the
/// storage plugin"; and a gear-side guard covers the in-process surface as
/// well as REST.
///
/// Walks the whole tree and both comparison shapes. An `in` list is a list
/// of literals and each is checked, because a guard inspecting only
/// `Compare` would serve the request from the admissible half of the list.
///
/// **Operator-blind, deliberately.** [`check_closed_pair`] never reads
/// `_op`, so `entry_type ne 'Record'` and `not (entry_type eq 'Record')` are
/// refused exactly like `entry_type eq 'Record'` — a predicate *naming* a
/// value outside the closed set is a confused request regardless of which
/// operator compares against it, and an operator-aware guard (admit `ne`
/// against an off-label value because no row could match it anyway; refuse
/// `eq`) would need to reason about every operator `toolkit_odata` admits
/// rather than just the literal, for a case `origin`/`entry_type` are
/// unlikely to need. This is a known, accepted narrowing: before this guard
/// existed, `origin ne 'imported'` returned a **semantically correct full
/// page** (no row can hold `'imported'` under the `CHECK` constraint), and
/// it now draws a 400 instead — a permit→deny on `origin ne`/`not (origin
/// eq …)` specifically, since `entry_type`'s analogous case was already a
/// `22P02` 500 the guard strictly improves on. Pinned by
/// `an_off_label_origin_not_equals_is_refused_though_the_old_answer_was_a_correct_page`.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] naming the offending value and
/// the labels the field admits.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
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
/// Published in the `POST /records/aggregate` operation description
/// (`usage-collector-v1.yaml`) as prose, not `maxItems` — `OpenAPI` cannot
/// declare `maxItems` on a parameter whose name carries a placeholder, and
/// `AggregationRequest` carries no `metadata_filter` body array that could
/// carry this cap as a machine-checkable `maxItems: 16`.
/// This constant is the cap's only enforcement, and it lives here rather than
/// in the REST handler because this module is the one point every surface
/// passes through — REST, the in-process client and a direct `Service` call
/// alike. Enforcing it at the REST edge alone would leave an in-process caller
/// uncapped.
pub(crate) const MAX_METADATA_FILTERS: usize = 16;

/// The most values one `metadata_filter` predicate admits.
///
/// Published as `maxItems: 32` on `MetadataFilter.values` — still true as
/// text, but `MetadataFilter` is no longer reachable by `$ref` from either
/// wire path (`AggregationRequest` dropped its last reference when it was
/// narrowed), so this is the same correction [`MAX_METADATA_FILTERS`]
/// received four lines above: this constant is the cap's only *enforcement*,
/// the yaml's `maxItems: 32` is now unreachable prose rather than a schema
/// check, and the REST handler's empty-key / empty-values parsing described
/// on `usage-collector-v1.yaml`'s aggregate operation is what actually runs
/// against a caller, not this component.
pub(crate) const MAX_METADATA_FILTER_VALUES: usize = 32;

/// Requires a `metadata_filter` to be within the published caps.
///
/// Two bounds, and they are on the wire contract rather than on one path's
/// machinery, so this runs on **both** read paths. On the raw path it has a
/// second job: the predicate count and value count are what keep a minted
/// cursor inside the published `maxLength: 4096`, since the fingerprint the
/// token carries is computed over them.
///
/// One enforcement point, not two. The REST handler's own copy of these
/// checks is removed rather than kept as an early twin: one point cannot
/// diverge from itself, and two can.
///
/// The wire field names are the REST handler's — `metadata` for the count
/// and `metadata.<key>` for the values — so moving the check does not move
/// the error a caller sees.
///
/// **Entries, not distinct keys.** `metadata_filter.len()` counts slice
/// entries. `&[MetadataFilter]` can carry the same key twice, and
/// [`MetadataFilter`]'s own doc says two entries naming one key are AND-ed
/// and "not equivalent to the single filter `k in {a, b}`" — so counting
/// entries is slightly stricter than the REST handler's old count, which
/// grouped `metadata.<key>` parameters into a map first and so counted
/// distinct keys. A caller sending the same key twice therefore hits this
/// cap one entry sooner than the old REST-only count would have. That is
/// the right count for the in-process shape regardless: it is what bounds
/// the fingerprint (`crate::domain::list_query::read_fingerprint_pre_image`
/// renders one field per entry, post-dedup only for *wholly identical*
/// entries) and the SQL a plugin builds, not the count of distinct keys named.
///
/// # Errors
///
/// [`UsageCollectorError::InvalidArgument`] naming the cap that was
/// exceeded and by how much.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
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
/// moment ago is usable on the very next call, per Spec §3.11.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] (via
/// [`UsageCollectorError::unknown_metadata_key`]) naming the first
/// undeclared key.
///
/// Traceability: the `metadata_filter` half of
/// `cpt-cf-usage-collector-algo-query-field-validation` /
/// `cpt-cf-usage-collector-dod-query-field-validation` ("a metadata
/// predicate key MUST be a property the resolved declaration declares").
// @cpt-algo:cpt-cf-usage-collector-algo-query-field-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-query-field-validation:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
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
/// bump, per usage-query.md `inst-*-inflight-increment` / `-telemetry-complete`.
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
