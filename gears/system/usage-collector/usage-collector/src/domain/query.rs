//! Query read-path helpers for the usage-collector domain service.
//!
//! Holds the helpers serving only the `list_usage_records` /
//! `query_aggregated_usage_records` read paths, kept out of `service.rs` so it
//! stays focused on orchestration. Every `pub(crate)` item here is listed, in
//! the three groups the read paths call them in.
//!
//! **Scope composition**
//!
//! * `compose_query_with_scope` — AND-merges the PDP-returned
//!   [`AccessScope`] into the caller's `$filter`.
//!
//! **The Spec §3.11 surface gate.** The aggregate path runs every check below;
//! the raw path runs all but `require_dimensions_declared`, having no
//! `group_by` to check. The admissible surface is the published fixed
//! fields plus the queried meter's declared metadata keys, recomputed per
//! request from the resolved declaration and never cached independently of it,
//! so a property declared a moment ago is usable on the very next call.
//!
//! * `reject_unpublished_filter_fields` — rejects a `$filter` naming any field
//!   outside [`PUBLISHED_FILTER_FIELDS`], wherever in the AST it appears: a field
//!   reserved to a typed parameter (`gts_type_id`, the covered-period bounds)
//!   with its own reason, anything else off the set with a second.
//! * `reject_off_label_literals` — rejects a `$filter` comparing a closed-label
//!   field (`entry_type`, `origin`) against a literal outside its label set,
//!   which the backend would otherwise answer wrongly. Field membership and
//!   literal admissibility are two separate gates.
//! * `require_dimensions_declared` — checks a `group_by` list names only the
//!   fixed dimensions or a declared metadata key, each at most once. The
//!   aggregate path's alone: no other surface carries a `group_by`.
//! * `require_metadata_filter_keys_declared` — the declaredness check for
//!   `metadata_filter`, the dynamic-key side channel that exists because the
//!   `toolkit-odata` grammar cannot express filters over JSON map keys, so it
//!   never flows through `$filter` at all.
//! * `require_metadata_filter_within_caps` — caps `metadata_filter` at
//!   `MAX_METADATA_FILTERS` predicates of `MAX_METADATA_FILTER_VALUES` values
//!   each, here rather than in the REST handler, so an in-process caller is
//!   capped too.
//!
//! **The raw path's cursor lifecycle**, all gateway-owned
//! (`cpt-cf-usage-collector-dod-gateway-owned-cursor`): the plugin is handed a
//! structured [`Keyset`] and never a wire token.
//!
//! * `establish_keyset_order` / `require_continuation_keyset` — the keyset
//!   floor, in both its modes. Between them every dispatch carries the non-empty,
//!   uniform-direction, never-null, no-repeated-key order naming both canonical
//!   keyset fields that the Plugin SPI promises, whichever surface the call
//!   came in on. A first page has its order *established*; a continuation has
//!   the token's order *required* to be one already.
//! * `admit_continuation` — the funnel every cursor request passes through: it
//!   binds the token's order, then applies `require_continuation_keyset`,
//!   `require_forward_cursor` and `require_cursor_fingerprint` in that order
//!   (structure before relevance, binding before both).
//! * `require_forward_cursor` — refuses a token whose direction is not forward.
//!   `CursorV1::decode` only checks the field is one of the two admissible
//!   spellings, so this is where a forward-only read path says so.
//! * `read_fingerprint` / `require_cursor_fingerprint` — the query a keyset
//!   continuation is bound to (the caller's `$filter` plus `gts_type_id`, the
//!   read range and `metadata_filter`), and the refusal of a cursor minted over
//!   a different one.
//! * `keyset_from_cursor` — extracts the token's boundary values into the typed
//!   [`Keyset`] the SPI takes.
//! * `verify_returned_keyset` — checks what the plugin handed back before a
//!   token is minted from it, so a non-conforming keyset is a refused page
//!   rather than a silently misaligned next one.
//! * `mint_record_cursor` — mints the next page's wire token from that
//!   verified keyset.

use std::collections::{BTreeSet, HashSet};

use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, SortDir, ast};
use toolkit_security::AccessScope;
use usage_collector_sdk::{
    AggregationDimension, EntryType, Keyset, MetadataFilter, MeterTypeId, PUBLISHED_FILTER_FIELDS,
    RECORD_ID_FIELD, RecordOrigin, RecordPage, TimeRange, UsageCollectorError, WINDOW_END_FIELD,
    WINDOW_START_FIELD, is_keyset_safe_record_field,
};

use crate::domain::authz;
use crate::domain::fingerprint::fnv1a_64;

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
/// [`read_fingerprint`], but that is an opaque pagination fingerprint and
/// not a row constraint.)
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
/// [`require_dimensions_declared`]'s uniqueness pass argues, pointing the
/// other way: there an exact comparison keeps a case-differing *typo*
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

/// The fields every raw-list order must name for its sort tuple to be a
/// sound keyset.
///
/// `window_end` is the primary time key — the same column the read range
/// selects on (`cpt-cf-usage-collector-adr-window-end-selection`), so one
/// index serves both the selection and the page order. `id` is unique per
/// row, so an order naming it can never leave a page boundary inside a run
/// of rows sharing every other key.
///
/// These are *membership* requirements, not positions.
/// [`establish_keyset_order`] appends whichever is missing, so an order
/// naming neither ends in `(window_end, id)` — but a caller order that
/// already names `id` keeps it where it put it (`$orderby=id` becomes
/// `(id, window_end)`). What is guaranteed downstream is that both names
/// are present, which is what makes the tuple unique; where they sit is
/// not.
const CANONICAL_KEYSET_FIELDS: &[&str] = &[WINDOW_END_FIELD, RECORD_ID_FIELD];

/// Why an order cannot serve as a keyset, independent of which surface it
/// arrived on.
///
/// Both modes of the floor share these rules and differ only in what
/// they do about a shortfall, so the rules live here once and each mode
/// renders them into the error its own caller can act on.
enum KeysetDefect {
    /// The named key sorts against the leading key's direction.
    MixedDirection(String),
    /// The named key is not a never-null record attribute — or is not a
    /// record attribute at all, since
    /// [`is_keyset_safe_record_field`] is a fail-closed allowlist.
    InadmissibleKey(String),
    /// The named key is named more than once.
    DuplicateKey(String),
}

/// The properties an order must have before it can be a keyset at
/// all, none of which appending a field could repair.
///
/// * **One direction.** The plugin's continuation is a row-value tuple
///   comparison (`(c1, c2, …) > ($…)`), which has no meaning across mixed
///   directions.
/// * **No nullable key.** A tuple whose leading column is NULL compares as
///   NULL in SQL's three-valued logic, so every NULL-keyed row silently
///   drops out of the page and a page ending on one cannot encode a cursor
///   at all.
/// * **No repeated key.** Nothing downstream dedups an order:
///   `toolkit_odata::ODataOrderBy::ensure_tiebreaker` only *skips* a field
///   already named rather than deduplicating it out, and neither
///   `from_signed_tokens` nor the plugin's keyset rendering notices a
///   repeat either. A key named three times mints a keyset carrying three
///   boundary values and a cursor three signed tokens wider than a sound
///   one — `$orderby=resource_id,resource_id,resource_id` alone breaches
///   the published cursor `maxLength` before the canonical tiebreaker is
///   even appended. The ground is that budget, not a claimed
///   order-uniqueness rule: no document states one. This is the exact
///   mirror of the aggregate path's `group_by` uniqueness gap — two
///   surfaces, one missing dedup each.
///
/// **One defect is returned, and the order the three are checked in is
/// load-bearing** — the same property [`require_dimensions_declared`]
/// states for its own clauses, and for the same reason: each refusal
/// names one key, so when an order is defective two ways the caller is
/// told only one thing and it had better be the actionable one.
///
/// `DuplicateKey` is checked **last**. On `$orderby=nope,nope` both the
/// second and the third rule hold, and the two available answers are
/// *"`nope` is not a mandatory record attribute"* and *"you named it
/// twice"*. The first is the caller's actual mistake — deleting the
/// duplicate leaves `$orderby=nope`, still refused — while the second
/// sends them to fix the multiplicity of a name that was never orderable
/// at any multiplicity.
///
/// `MixedDirection` is checked **first**. Its ground is weaker than the
/// one above, since a direction defect and a key defect can each be
/// repaired on their own: it is a property of the tuple as a whole rather
/// than of any single key, so it is reported before the per-key rules
/// name a key. The position is pinned either way (`query_tests.rs`'s
/// `the_first_defect_reported_for_a_doubly_defective_order_is_the_actionable_one`),
/// so a silent reordering of these clauses fails a test rather than
/// quietly re-labelling a refusal.
fn keyset_defect(order: &ODataOrderBy) -> Option<KeysetDefect> {
    let keys = &order.0;
    if let Some(first) = keys.first()
        && let Some(deviating) = keys.iter().find(|key| key.dir != first.dir)
    {
        return Some(KeysetDefect::MixedDirection(deviating.field.clone()));
    }
    if let Some(bad) = keys
        .iter()
        .find(|key| !is_keyset_safe_record_field(&key.field))
    {
        return Some(KeysetDefect::InadmissibleKey(bad.field.clone()));
    }
    let mut seen = BTreeSet::new();
    keys.iter()
        .find(|key| !seen.insert(key.field.as_str()))
        .map(|dup| KeysetDefect::DuplicateKey(dup.field.clone()))
}

/// Establishes the keyset the Plugin SPI promises on a **first page**: a
/// non-empty, uniform-direction, never-null order that names every field
/// in [`CANONICAL_KEYSET_FIELDS`], and so sorts on a globally unique
/// tuple.
///
/// This is the raw path's keyset floor, and it lives here rather than in
/// the REST handler because DESIGN §3.1 allocates order admissibility to
/// the Query Gateway, which §3.2 exists to keep uniform across the SDK and
/// REST. An in-process caller reaches
/// [`Service::list_usage_records`](crate::domain::Service::list_usage_records)
/// with an [`ODataQuery`] of their own construction — an empty order, most
/// of the time — so a normalization that only ran at the REST edge left
/// the SPI's order slot unpopulated on exactly the surface no REST test
/// can reach.
///
/// [`keyset_defect`] must clear first, because neither of the properties
/// it checks can be repaired by appending. Whichever canonical field the
/// order does not already name is then appended in the order's own
/// direction (`Asc` for an empty order) via
/// [`toolkit_odata::ODataOrderBy::ensure_tiebreaker`], which skips a field
/// the order already names — so this is idempotent, and applying it after
/// REST has validated an order is a no-op rather than a double append. It
/// is also why the guarantee is about membership and not position: a
/// caller order already naming `id` keeps it where it is and gains only
/// `window_end` after it.
///
/// `prepare_list_query` calls this too, on the caller's `$orderby` before
/// any authorization or plugin work happens, so a wire request is refused
/// where its input is parsed and the `400` blames the parameter the caller
/// actually sent. That mirroring is the point of the idempotence: the same
/// rule, stated once, applied at the edge for the message and again in the
/// service for the guarantee.
///
/// A continuation goes to [`require_continuation_keyset`] instead — the
/// two are separate functions precisely so a call site says which one it
/// wants rather than letting the presence of a cursor decide silently.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] against `$orderby`
/// when the order mixes sort directions, names a key that is not a
/// mandatory record attribute, or names a key more than once.
///
/// Traceability: the raw path's half of
/// `cpt-cf-usage-collector-algo-query-field-validation` /
/// `cpt-cf-usage-collector-dod-query-field-validation` — the order-key
/// admissibility check ("validate every caller order key... against the
/// order key set the public contract defines").
// @cpt-algo:cpt-cf-usage-collector-algo-query-field-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-query-field-validation:p1
// @cpt-algo:cpt-cf-usage-collector-algo-query-cursor-lifecycle:p1
// @cpt-dod:cpt-cf-usage-collector-dod-gateway-owned-cursor:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn establish_keyset_order(query: &mut ODataQuery) -> Result<(), UsageCollectorError> {
    match keyset_defect(&query.order) {
        Some(KeysetDefect::MixedDirection(field)) => {
            return Err(UsageCollectorError::mixed_direction_order(&field));
        }
        Some(KeysetDefect::InadmissibleKey(field)) => {
            return Err(UsageCollectorError::inadmissible_order_key(&field));
        }
        Some(KeysetDefect::DuplicateKey(field)) => {
            return Err(UsageCollectorError::duplicate_order_key(&field));
        }
        None => {}
    }

    let dir = query.order.0.last().map_or(SortDir::Asc, |key| key.dir);
    let mut order = std::mem::take(&mut query.order);
    for &field in CANONICAL_KEYSET_FIELDS {
        order = order.ensure_tiebreaker(field, dir);
    }
    query.order = order;
    Ok(())
}

/// Admits a continuation, or refuses it: the rules a cursor request must
/// satisfy before its query reaches a plugin.
///
/// One call site, because they all constrain the same artifact and a caller
/// cannot satisfy some of them:
///
/// 1. [`bind_continuation_order`] replaces the order with the one the
///    token was minted under, so the sort the plugin performs and the
///    boundary values it compares against come from the same place.
/// 2. [`require_continuation_keyset`] refuses that order if it is not a
///    sound keyset — checking it, never extending it.
/// 3. [`require_forward_cursor`] refuses a token whose `d` is not
///    `"fwd"`. `toolkit_odata::CursorV1::decode` only checks `d` is one of
///    `{"fwd", "bwd"}`, so this is the one place a read path that supports
///    forward paging alone gets to say so.
/// 4. [`require_cursor_fingerprint`] refuses a token minted over a
///    different query.
///
/// They stay separate functions, each with its own upstream error and its
/// own tests, because they are actionable differently: 2 and 3 say the
/// token is structurally unusable, 4 says it belongs to another query.
/// Structure before relevance, and binding before both — a rule about the
/// order cannot be applied to an order that has not been established yet.
///
/// The gear originates neither wire code. Each refusal carries the
/// `toolkit_odata` error that owns it — `InvalidCursor` for structure,
/// `FilterMismatch` for relevance — and the host lift converts that error
/// to obtain the wire code the caller reads (Spec §3.13). The wire field is
/// not upstream's: it is the gear's own `CursorField`, naming which of the
/// feed's two cursor-bearing parameters the refusal is about (DESIGN §3.3).
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] from whichever rule
/// refuses first.
///
/// Traceability: the decode/validate half of
/// `cpt-cf-usage-collector-algo-query-cursor-lifecycle` /
/// `cpt-cf-usage-collector-dod-gateway-owned-cursor` — every rule a
/// caller-supplied continuation must satisfy runs here, in the gateway,
/// before a structured keyset ever reaches the plugin.
// @cpt-algo:cpt-cf-usage-collector-algo-query-cursor-lifecycle:p1
// @cpt-dod:cpt-cf-usage-collector-dod-gateway-owned-cursor:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn admit_continuation(
    query: &mut ODataQuery,
    fingerprint: &str,
) -> Result<(), UsageCollectorError> {
    bind_continuation_order(query)?;
    require_continuation_keyset(query)?;
    let cursor = query
        .cursor
        .as_ref()
        .ok_or_else(|| UsageCollectorError::inadmissible_cursor_keyset("it carries no cursor"))?;
    require_forward_cursor(cursor)?;
    require_cursor_fingerprint(cursor, fingerprint)
}

/// Replaces `query.order` with the order the continuation token was minted
/// under, decoded from its own signed tokens.
///
/// The plugin reads `query.order` to build **both** the `ORDER BY` and the
/// keyset continuation predicate, and compares that predicate against the
/// boundary values in `CursorV1::k` — which are one per key **of the order
/// the token was minted under**. So the two have to be the same order, and
/// the token is the only one of the two that cannot have been tampered
/// with independently: `k` and `s` travel together.
///
/// A caller-supplied order on a cursor request is therefore not an input,
/// it is a contradiction, and it is overwritten rather than compared.
/// Overwriting is also what makes this safe to state as a guarantee: an
/// order that merely *agreed* would still leave the caller's spelling in
/// the slot, and nothing downstream re-derives it.
///
/// This is the half of [`toolkit_odata::validate_cursor_against`] that
/// concerns the order, and it lives here for the same reason the
/// fingerprint does: the REST extractor leaves `query.order` empty on a
/// cursor request, so the handler's own derivation happens to be
/// equivalent — but an **in-process** caller sets `order` and `cursor`
/// independently, and an edge-only derivation left that caller able to hand
/// the plugin `(id, window_end)` against boundary values ordered
/// `(window_end, id)`. Both are sound keysets naming both canonical
/// fields, so no structural check catches it, and the result is a
/// misaligned continuation served as a `200`.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `InvalidCursor` when the token's signed-token payload
/// does not decode into a non-empty order. That is a malformed token — the
/// same class as a truncated or forged one — so it is refused in the
/// cursor's own vocabulary, upstream's, rather than surfacing as an
/// `$orderby` complaint about a parameter the caller cannot send alongside
/// a cursor. The wire code follows from that upstream error and is not
/// spelled by this gear (Spec §3.13); the wire field is `CursorField::Cursor`,
/// spelled here rather than read off upstream.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn bind_continuation_order(query: &mut ODataQuery) -> Result<(), UsageCollectorError> {
    let Some(cursor) = query.cursor.as_ref() else {
        return Ok(());
    };
    match ODataOrderBy::from_signed_tokens(&cursor.s) {
        Ok(order) => {
            query.order = order;
            Ok(())
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                signed_tokens = %cursor.s,
                "usage-collector refused a continuation token whose signed keys do not decode"
            );
            Err(UsageCollectorError::inadmissible_cursor_keyset(format!(
                "its signed keys do not decode into an order ({err})"
            )))
        }
    }
}

/// Requires the order of a **continuation** to be a sound keyset already,
/// rather than making it one.
///
/// A cursor request's order does not come from the caller: it has been
/// reconstructed from the token's own signed keys by
/// [`bind_continuation_order`], which runs immediately before this and is
/// what makes that sentence true on every surface rather than only over
/// REST. The token's boundary values (`CursorV1::k`) line up with it one
/// for one. There is nothing
/// left to normalize — the order either already is the keyset the page was
/// minted under, or the token did not come from a conforming plugin.
/// Appending to it would widen the sort tuple past the boundary values the
/// token carries and hand the plugin a misaligned continuation, which is a
/// silently wrong page where refusing is merely a refused one. Nothing
/// downstream would catch it either:
/// [`toolkit_odata::validate_cursor_against`] never checks the token's
/// width against the order's, and this gear hands it no filter hash at all
/// — whether a token belongs to this query is
/// [`require_cursor_fingerprint`]'s question, not the toolkit's.
///
/// Taking `&ODataQuery` rather than `&mut` is the point — the signature is
/// what says this path appends nothing.
///
/// A conforming plugin cannot trip this. It mints `next_cursor` from the
/// order it was handed, which is a floored one, and
/// `to_signed_tokens` / `from_signed_tokens` round-trip field names and
/// directions exactly — so the reconstructed order passes and this is a
/// no-op. Everything refused here is a forged, truncated or replayed token,
/// or a plugin minting against an order it was not given, which is why the
/// refusal also logs.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `InvalidCursor` — the sole source of the wire code
/// (Spec §3.13), while the wire field is `CursorField::Cursor`, the gear's
/// own — when the order mixes sort directions, names a key that is not a
/// mandatory record attribute, names a key more than once, or does not
/// already name every [`CANONICAL_KEYSET_FIELDS`] entry.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn require_continuation_keyset(query: &ODataQuery) -> Result<(), UsageCollectorError> {
    let defect = match keyset_defect(&query.order) {
        Some(KeysetDefect::MixedDirection(field)) => {
            format!("its key '{field}' sorts against the leading key")
        }
        Some(KeysetDefect::InadmissibleKey(field)) => {
            format!("its key '{field}' is not a mandatory record attribute")
        }
        Some(KeysetDefect::DuplicateKey(field)) => {
            format!("its key '{field}' is named more than once")
        }
        None => match CANONICAL_KEYSET_FIELDS
            .iter()
            .find(|field| !query.order.0.iter().any(|key| key.field == **field))
        {
            Some(missing) => format!("it does not name '{missing}'"),
            None => return Ok(()),
        },
    };

    // A caller-visible 400, but the likeliest cause is a plugin minting a
    // `next_cursor` against an order it was not handed — a conformance
    // breach the caller can do nothing about and would otherwise leave no
    // trace. `warn!` rather than the `error!` of a host-invariant breach:
    // a forged or replayed token reaches here too, and that is ordinary
    // hostile input. Same shape as `service::invariant_breach` — log the
    // detail, then return the typed error.
    //
    // The defect and the `signed_tokens` beside it describe the same
    // thing, because `bind_continuation_order` derived the order under
    // inspection from exactly those tokens. Before it did, an in-process
    // caller could produce a log line showing a healthy token next to a
    // defect that came from the caller's own contradictory `order`.
    tracing::warn!(
        defect = %defect,
        signed_tokens = %query
            .cursor
            .as_ref()
            .map_or("<none>", |cursor| cursor.s.as_str()),
        "usage-collector refused a continuation token whose bound order is not a keyset"
    );
    Err(UsageCollectorError::inadmissible_cursor_keyset(defect))
}

/// Refuses a continuation whose `d` is not `"fwd"`.
///
/// `admit_continuation`'s other rules bind and check the *order*; none
/// reads `cursor.d`. `toolkit_odata`'s `CursorV1::decode` validates only that
/// `d` is one of `{"fwd", "bwd"}` — it has no notion of which directions a read
/// path supports — so without this guard a well-formed, correctly-bound,
/// correctly-fingerprinted `d: "bwd"` token decodes clean and is served a
/// forward page with `200`. The raw path mints `d: "fwd"` exclusively
/// ([`mint_record_cursor`]) and is forward-only by design
/// (`usage-collector-v1.yaml`'s `Cursor` parameter), so nothing legitimate
/// ever carries `"bwd"` here; a token that does is forged, replayed from
/// another surface, or hand-built by a caller testing the boundary.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `InvalidCursor` when `cursor.d` is not `"fwd"`.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn require_forward_cursor(cursor: &CursorV1) -> Result<(), UsageCollectorError> {
    if cursor.d == "fwd" {
        return Ok(());
    }
    tracing::warn!(
        direction = %cursor.d,
        "usage-collector refused a continuation token whose direction is not forward"
    );
    Err(UsageCollectorError::unsupported_cursor_direction(format!(
        "it specifies direction `{}`; the raw path reads forward only",
        cursor.d
    )))
}

/// The fingerprint a keyset continuation is bound to: a 16-character digest
/// of every input that decides which rows the page came from.
///
/// Those inputs are the caller's `$filter` plus all three typed parameters
/// — `gts_type_id`, the read range, and `metadata_filter`. None of the
/// three is a `$filter` conjunct, so `toolkit_odata::short_filter_hash`
/// sees none of them and each has to enter here explicitly.
///
/// `CursorV1::f` and [`ODataQuery::filter_hash`] exist so a caller who
/// changes their query between pages is refused rather than served a
/// continuation minted over a different row set. While the mandatory
/// window lived inside `$filter` the hash covered it for free; it does not
/// any more, and it never covered the meter or the metadata filter. Without
/// all four a page-2 request can carry the same cursor against a different
/// range, a different `metadata.<key>` value, or **another meter entirely**
/// and be served a continuation that means nothing over its own row set —
/// as a `200` nothing downstream can notice.
///
/// # Why a digest and not the pre-image
///
/// The value is serialized into `CursorV1::f`, base64url-encoded into the
/// opaque `cursor` token, and sent back as a **URL query parameter**. So
/// its length is a wire constraint, and returning the pre-image made the
/// cursor grow with the caller's own metadata filter: [`MAX_METADATA_FILTERS`]
/// and [`MAX_METADATA_FILTER_VALUES`] cap how *many* keys and values a caller
/// may send, not how long they are, so a request well inside both caps —
/// five keys of twenty UUID values, say — yields a multi-kilobyte cursor
/// and a page-2 URL that a proxy refuses with a `414` the caller cannot act
/// on. Page one succeeds, page two does not, and nothing in the gear is
/// involved in the failure. Hashing makes the value's length a constant of
/// this function instead of a function of the caller's input.
///
/// Correctness is unaffected either way, which is why the size argument stands
/// on its own: a digest compares equal precisely when the pre-image does, up to
/// collision, and [`read_fingerprint_pre_image`] rules the collisions out.
///
/// [`fnv1a_64`] runs the same algorithm, rendered the same 16 hex digits, as
/// the `short_filter_hash` that produced the `$filter` field nested inside the
/// pre-image — one hashing primitive rather than two. FNV-1a is a fixed public
/// specification, so the digest is stable across builds, platforms and
/// replicas, which a value compared on a *later* request cannot do without.
///
/// # Ownership
///
/// Computed from the **caller's** query, never the composed one — see
/// [`compose_query_with_scope`], which states why the server-injected PDP scope
/// must stay out of this value.
///
/// The value round-trips across a page boundary without ever reaching a
/// plugin: the read path computes it here from the caller's query, holds it
/// locally, and [`mint_record_cursor`] writes it straight into the token's
/// `f` — `composed.filter_hash` (the caller's own, that
/// [`compose_query_with_scope`] preserved) is stripped to `None` before
/// dispatch rather than overwritten with this value, since
/// `UsageCollectorPluginV1::list_usage_records` states a plugin "MUST NOT"
/// read the slot. The follow-up request recomputes the same string from its
/// own parameters and [`require_cursor_fingerprint`] compares it against
/// the token's `f` directly.
pub(crate) fn read_fingerprint(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    user_query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> String {
    let pre_image =
        read_fingerprint_pre_image(gts_type_id, time_range, user_query, metadata_filter);
    format!("{:016x}", fnv1a_64(pre_image.as_bytes()))
}

/// Mints the raw path's wire cursor from the keyset a plugin returned.
///
/// The raw-path twin of [`crate::domain::feed::mint_cursor`], and it exists
/// for the same reason: `cpt-cf-usage-collector-dod-gateway-owned-cursor`
/// puts minting, decoding and validating the token in the gateway, and the
/// plugin returns a [`Keyset`] instead. Before slice 7 the plugin minted
/// this token and the gear passed it through.
///
/// `s` carries the order's signed tokens, which is what
/// [`crate::domain::query::admit_continuation`] reconstructs the order from
/// on the way back, so the two have to be the same order — hence `order` is
/// the one the read was *dispatched* under rather than the one the caller
/// sent. `f` is the read fingerprint over the caller's `$filter` and all
/// three typed parameters, which [`require_cursor_fingerprint`] checks on
/// the follow-up.
///
/// Traceability: the mint half of
/// `cpt-cf-usage-collector-algo-query-cursor-lifecycle` /
/// `cpt-cf-usage-collector-dod-gateway-owned-cursor`.
// @cpt-algo:cpt-cf-usage-collector-algo-query-cursor-lifecycle:p1
// @cpt-dod:cpt-cf-usage-collector-dod-gateway-owned-cursor:p1
pub(crate) fn mint_record_cursor(
    order: &ODataOrderBy,
    keyset: &Keyset,
    fingerprint: &str,
) -> CursorV1 {
    CursorV1 {
        k: keyset.values().to_vec(),
        o: keyset.direction(),
        s: order.to_signed_tokens(),
        f: Some(fingerprint.to_owned()),
        // DESIGN §3.3's `Cursor` parameter: "Forward-only".
        d: "fwd".to_owned(),
    }
}

/// Extracts the structured keyset a continuation token carries, for dispatch.
///
/// Runs after [`admit_continuation`], which has already reconstructed
/// `query.order` from the token's own signed keys and checked that order is
/// a sound keyset — so `cursor.k` is positionally aligned with it by the
/// time this is called, and the width check here is the one thing left that
/// the token itself could have got wrong.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] when the token's
/// boundary values do not number one per key of the bound order. That is a
/// forged, truncated or replayed token rather than caller input on a
/// parameter they can change, so it is refused in the cursor's own
/// vocabulary.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn keyset_from_cursor(
    cursor: &CursorV1,
    order: &ODataOrderBy,
) -> Result<Keyset, UsageCollectorError> {
    if cursor.k.len() != order.0.len() {
        return Err(UsageCollectorError::inadmissible_cursor_keyset(format!(
            "it carries {} boundary values for an order of {} keys",
            cursor.k.len(),
            order.0.len()
        )));
    }
    Keyset::new(cursor.k.clone(), cursor.o)
        .map_err(|err| UsageCollectorError::inadmissible_cursor_keyset(err.to_string()))
}

/// Verifies a plugin's returned page against the order it was dispatched
/// under, before the gateway mints a token from it.
///
/// Properties the type system does not carry: a continuation
/// has one boundary value per key of the dispatched order, it sorts in that
/// order's direction, and it belongs to a page that actually has a last
/// row. The gateway mints from this value, so a breach here becomes a token
/// a caller follows — which is why each is refused rather than logged.
///
/// `internal` in every arm, on the distinction
/// [`crate::domain::Service::read_usage_feed`] already draws for the feed's
/// over-length page: the aggregate path's bucket cap is `InvalidArgument`
/// because a caller reaches it by widening `group_by`, whereas no request a
/// caller can send avoids a plugin returning a malformed keyset. It is a
/// host-contract breach.
///
/// The direction check reads `order`'s leading key directly rather than
/// going through the plugin's `uniform_dir` (`keyset.rs`, slice 7 task 2
/// finding H27, which resolved the same "trust the leading key" question
/// the other way). The sites differ because they answer different
/// questions: `uniform_dir` establishes that an `order` handed to the
/// plugin *is* uniform-direction at all, which a pure function taking
/// `&ODataOrderBy` in isolation cannot assume — mixed-direction input is
/// exactly what it exists to catch. By the time `order` reaches here it has
/// already passed [`establish_keyset_order`] or [`require_continuation_keyset`]
/// on every surface, so its uniformity is not this function's question;
/// what is under test is only whether the *returned keyset* agrees with the
/// order's one, already-established direction.
///
/// # Errors
///
/// [`UsageCollectorError::Internal`] naming the mismatch.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn verify_returned_keyset(
    order: &ODataOrderBy,
    page: &RecordPage,
) -> Result<(), UsageCollectorError> {
    let Some(keyset) = page.next.as_ref() else {
        return Ok(());
    };
    if page.items.is_empty() {
        return Err(UsageCollectorError::internal(
            "usage-collector storage plugin returned an empty page carrying a continuation \
             keyset; a continuation names the last row of the page it continues, and a caller \
             following this one would page forever",
        ));
    }
    if keyset.values().len() != order.0.len() {
        return Err(UsageCollectorError::internal(format!(
            "usage-collector storage plugin returned {} keyset boundary values for the {}-key \
             order it was dispatched under; a token minted from this would bind an order its \
             own boundary values cannot line up with",
            keyset.values().len(),
            order.0.len(),
        )));
    }
    // Closed, not defaulted: a 0-key `order` can never reach here in the
    // first place — `Keyset::new` refuses an empty `values`, so the arity
    // check above already refuses every dispatch whose order is shorter
    // than the keyset's one-or-more values — but a guard that invented
    // `Asc` for an order it cannot read a direction from would be the wrong
    // shape for a function whose whole job is refusing what it cannot
    // verify. Reaching `None` here is this function's own invariant broken,
    // not caller or plugin input, hence `internal` rather than silently
    // picking a direction to compare against.
    let Some(first) = order.0.first() else {
        return Err(UsageCollectorError::internal(
            "usage-collector dispatched an empty order against a non-empty returned keyset; \
             the arity check above should have refused this first, so reaching here is a bug \
             in verify_returned_keyset's own invariants",
        ));
    };
    if keyset.direction() != first.dir {
        return Err(UsageCollectorError::internal(format!(
            "usage-collector storage plugin returned a keyset sorting {:?} against the {:?} \
             order it was dispatched under",
            keyset.direction(),
            first.dir,
        )));
    }
    Ok(())
}

/// The exact bytes [`read_fingerprint`] digests.
///
/// Split out from the digest so a change to the rendering is diagnosable:
/// a golden test pins this string, and a second pins the digest over it, so
/// an edit that moves the fingerprint says *which layer* moved instead of
/// only "the hash changed".
///
/// Every field is length-prefixed as `<len>:<bytes>`, making the
/// concatenation self-delimiting — a netstring. That is load-bearing, and
/// **more** so under hashing rather than less: a collision here is a
/// collision in the digest, and nothing downstream could tell the two
/// queries apart. None of the fields is separator-free, so no single
/// separator would do. A GTS type reference carries `~` as its own
/// terminator, [`TimeRange::canonical_form`] joins its two bounds with `~`,
/// and a metadata key or value is domain-opaque — `MetadataKey::new`
/// rejects only the empty string and NUL, so a key may contain any other
/// byte and a value is unconstrained. Whatever character were chosen, one
/// field's content could imitate a field boundary and two different
/// queries would render alike, so a cursor minted under one would validate
/// against the other. A decimal length makes injectivity hold for
/// arbitrary content instead of resting on a claim about what callers send.
///
/// The two count fields are part of that, not decoration: without the
/// per-entry value count, `[a → {b}, c → {d}]` and `[a → {b, c, d}]`
/// render identically.
///
/// `metadata_filter` is **normalized** before it is rendered, because the
/// fingerprint has to be a function of the query's meaning and not of how
/// the caller happened to spell it. A REST caller's repeated
/// `metadata.<key>` parameters reach `MetadataFilter::values` in
/// query-string order, duplicates included (`parse_metadata_filters` groups
/// through a `BTreeMap`, so it sorts keys but pushes values as they
/// arrive), and an in-process caller can build the slice in any order at
/// all. Since the semantics are OR within a key and AND across every
/// filter, two spellings that differ only in order or in a repeated value
/// are the same query — so values are sorted and deduplicated, entries
/// sorted by key, and wholly identical entries collapsed (`X AND X` is
/// `X`). Two entries on the same key with *different* value sets are
/// deliberately left as two: the storage plugin emits one AND-ed clause per
/// entry, so `k in {a} AND k in {b}` is not `k in {a, b}`.
fn read_fingerprint_pre_image(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    user_query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
) -> String {
    // `short_filter_hash` returns `None` for an absent filter, and an
    // absent filter is a legitimate complete request — so it folds in as
    // the empty string rather than short-circuiting the rest out of the
    // pre-image.
    let filter = toolkit_odata::short_filter_hash(user_query.filter()).unwrap_or_default();

    let mut entries: Vec<(&str, Vec<&str>)> = metadata_filter
        .iter()
        .map(|filter| {
            let mut values: Vec<&str> = filter.values().iter().map(String::as_str).collect();
            values.sort_unstable();
            values.dedup();
            (filter.key().as_str(), values)
        })
        .collect();
    entries.sort_unstable();
    // Only wholly identical entries collapse, which is why this runs after
    // the value normalization above: `X AND X` is `X`, while two entries on
    // one key with different value sets mean something a merge would lose.
    entries.dedup();

    let mut out = String::new();
    push_fingerprint_field(&mut out, gts_type_id.as_str());
    push_fingerprint_field(&mut out, &filter);
    push_fingerprint_field(&mut out, &time_range.canonical_form());
    push_fingerprint_field(&mut out, &entries.len().to_string());
    for (key, values) in entries {
        push_fingerprint_field(&mut out, key);
        push_fingerprint_field(&mut out, &values.len().to_string());
        for value in values {
            push_fingerprint_field(&mut out, value);
        }
    }
    out
}

/// Append one length-prefixed `<len>:<bytes>` field to a fingerprint
/// pre-image. See [`read_fingerprint_pre_image`] for why the length is
/// there.
fn push_fingerprint_field(out: &mut String, field: &str) {
    out.push_str(&field.len().to_string());
    out.push(':');
    out.push_str(field);
}

/// Requires a continuation token to have been minted over the query now
/// carrying it.
///
/// Refuses a token whose `f` is absent as well as one that differs.
/// [`toolkit_odata::validate_cursor_against`] skips its own comparison
/// whenever either side is `None`, which is exactly the hole this exists to
/// close: the cursor is caller-supplied JSON, so "no fingerprint recorded"
/// is not evidence of a matching query. [`mint_record_cursor`] always sets
/// `f: Some(fingerprint)` on the raw path's own mint, first page included
/// — the gear mints, never a plugin, so there is no implementor to forget
/// the field — which is why `f: None` reaching here names a forged,
/// truncated or hand-built token rather than an ordinary gap. (Only the raw
/// path mints at all: the aggregate path paginates nothing, so it needs no
/// fingerprint.)
///
/// Separate from [`require_continuation_keyset`] because the two answer
/// different questions about the same token and are actionable differently:
/// that one asks whether the token's order could be a keyset at all
/// (structure, `toolkit_odata`'s `InvalidCursor`), this one whether the
/// token belongs to this query (relevance, its `FilterMismatch`). Both run
/// on the continuation branch, structure first.
///
/// # Errors
///
/// Returns [`UsageCollectorError::CursorRejected`] carrying
/// `toolkit_odata`'s `FilterMismatch` — the sole source of the wire code
/// (Spec §3.13), while the wire field is `CursorField::Cursor`, the gear's
/// own — when the token carries no fingerprint or a different one.
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn require_cursor_fingerprint(
    cursor: &CursorV1,
    fingerprint: &str,
) -> Result<(), UsageCollectorError> {
    match cursor.f.as_deref() {
        Some(bound) if bound == fingerprint => Ok(()),
        // Absent, not merely different. `mint_record_cursor` always sets
        // `f: Some(fingerprint)` — the gear mints every raw-path token
        // itself, so there is no implementor step that could skip the
        // field — so `None` here is a forged, truncated or hand-built
        // token rather than a gap a conforming mint could leave. It still
        // refuses: the cursor is caller-supplied JSON, so an absent
        // fingerprint is not evidence of a matching query.
        //
        // Logged for the same reason `require_continuation_keyset` logs: a
        // mismatch is ordinary hostile or stale input, not a host-invariant
        // breach, so `warn!` rather than `error!`.
        bound => {
            tracing::warn!(
                bound_fingerprint = bound.unwrap_or("<none>"),
                expected_fingerprint = %fingerprint,
                likely_cause = if bound.is_none() {
                    "forged, truncated or hand-built token: a token this gear minted always carries f"
                } else {
                    "caller changed the query between pages, or the token was forged"
                },
                "usage-collector refused a continuation token bound to another query"
            );
            Err(UsageCollectorError::cursor_query_mismatch())
        }
    }
}

/// Checks every `group_by` dimension is either a fixed field (no
/// declaration needed) or a metadata property `declared_keys` actually
/// declares, and that no dimension is named more than once.
///
/// `declared_keys` is read from the resolved declaration fresh for this
/// request (see [`crate::domain::type_resolver::CompiledMetadataSchema::declared_keys`]),
/// never cached independently of it — so a property declared a moment ago
/// is usable on the very next call, per Spec §3.11. `gts_type_id` is the
/// queried meter, carried onto the error for operator-log parity with
/// [`UsageCollectorError::unknown_metadata_key`].
///
/// There is no ceiling on how many dimensions a `group_by` may carry:
/// DESIGN's `AggregationDimension` row says arity is "never ... bounded by
/// a fixed ceiling on how many a request may carry", PRD §5.1 admits
/// dimensions "in any combination and any order", and the yaml's
/// `group_by` property carries `uniqueItems: true` and no `maxItems`. What
/// bounds the result is the aggregation result limit and the mandatory
/// time range, not dimension count — pinned by
/// `every_admissible_dimension_once_in_an_arbitrary_order_is_admitted` in
/// `query_tests.rs`.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] naming the first
/// undeclared metadata dimension, checked before uniqueness: a metadata key
/// differing only in case (e.g. `Region` vs `region`) is a typo rather than
/// a repeat, since declaredness is an exact `BTreeSet<String>` match. A
/// case-insensitive uniqueness pass run first would refuse that pair as a
/// duplicate and tell the caller to delete one when what they have is a
/// misspelling — the undeclared-key error is the actionable one and must
/// win. Once every dimension is declared, [`UsageCollectorError::InvalidArgument`]
/// (via [`UsageCollectorError::repeated_grouping_dimension`]) names the
/// first dimension that repeats an earlier one, by exact equality.
///
/// Traceability: the `group_by` half of
/// `cpt-cf-usage-collector-algo-query-field-validation` /
/// `cpt-cf-usage-collector-dod-query-field-validation` ("a grouping
/// dimension MUST be one of the five fixed dimensions or a declared
/// property, MUST appear at most once").
// @cpt-algo:cpt-cf-usage-collector-algo-query-field-validation:p1
// @cpt-dod:cpt-cf-usage-collector-dod-query-field-validation:p1
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is large because Conflict carries reason-specific payload; callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
pub(crate) fn require_dimensions_declared(
    dimensions: &[AggregationDimension],
    declared_keys: &BTreeSet<String>,
    gts_type_id: &MeterTypeId,
) -> Result<(), UsageCollectorError> {
    // Declaredness first, and the order is load-bearing. Each refusal names
    // one dimension, so `[Region, region]` against a declaration carrying
    // only `region` has two available answers: "Region is undeclared" and
    // "these two repeat". The first is the caller's actual mistake — the
    // second would tell them to delete a duplicate when what they have is a
    // typo — and an undeclared name is wrong whatever its multiplicity.
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

    // Each at most once (DESIGN §3.1's AggregationDimension row,
    // dod-query-field-validation, and the yaml's `uniqueItems: true`).
    // Exact equality, deliberately: declaredness above is an exact
    // `BTreeSet` match, so two spellings differing in case are two
    // different keys and one of them has already been refused as
    // undeclared. A case-insensitive comparison here would disagree with
    // the check above and refuse a typo as a duplicate.
    //
    // There is NO ceiling on how many dimensions a request carries —
    // DESIGN says "never by a fixed ceiling", and what bounds the result is
    // the aggregation result limit and the mandatory range. The absence is
    // pinned by `every_admissible_dimension_once_in_an_arbitrary_order_is_admitted`.
    let mut seen: HashSet<&AggregationDimension> = HashSet::new();
    for dim in dimensions {
        if !seen.insert(dim) {
            return Err(UsageCollectorError::repeated_grouping_dimension(
                gts_type_id,
                dim,
            ));
        }
    }
    Ok(())
}

/// The most distinct `metadata_filter` predicates one read admits.
///
/// Published in the `POST /records/aggregate` operation description
/// (`usage-collector-v1.yaml`) as prose, not `maxItems` — `OpenAPI` cannot
/// declare `maxItems` on a parameter whose name carries a placeholder, and
/// `AggregationRequest` was narrowed to drop the `metadata_filter` body
/// array that used to carry this cap as a machine-checkable `maxItems: 16`.
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
/// narrowed), so this is the same H43 correction [`MAX_METADATA_FILTERS`]
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
/// checks is removed rather than kept as an early twin, on ruling G39's
/// ground: one point cannot diverge from itself, and two can.
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
/// the fingerprint ([`read_fingerprint_pre_image`] renders one field per
/// entry, post-dedup only for *wholly identical* entries) and the SQL a
/// plugin builds, not the count of distinct keys named.
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "query_tests.rs"]
mod query_tests;
