//! Unit tests for [`compose_query_with_scope`], for
//! [`reject_reserved_filter_fields`] / [`require_dimensions_declared`] /
//! [`require_metadata_filter_keys_declared`] — the Spec §3.11 gate on the
//! admissible `$filter` / `group_by` / `metadata_filter` surface — and for
//! [`establish_keyset_order`] / [`require_continuation_keyset`], the raw
//! path's keyset floor in its two modes, and for [`read_fingerprint`] /
//! [`require_cursor_fingerprint`], the query a continuation is bound to.
//!
//! There is no bounded-window guard to test: the mandatory read range is a
//! typed [`usage_collector_sdk::TimeRange`] parameter on both read paths
//! rather than a `$filter` conjunct, so nothing about it can be absent,
//! one-sided, or hidden under an `or`.

use std::collections::BTreeSet;

use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use usage_collector_sdk::{
    AggregationDimension, MetadataFilter, MetadataKey, MeterTypeId, UsageCollectorError,
    ValidationReason, is_keyset_safe_record_field,
};
use uuid::Uuid;

use super::{
    CANONICAL_KEYSET_FIELDS, compose_query_with_scope, establish_keyset_order, read_fingerprint,
    reject_reserved_filter_fields, require_continuation_keyset, require_cursor_fingerprint,
    require_dimensions_declared, require_metadata_filter_keys_declared,
};

/// Build an [`ODataQuery`] whose `$filter` is the parsed `filter` string.
fn query_with_filter(filter: &str) -> ODataQuery {
    let expr = toolkit_odata::parse_filter_string(filter)
        .expect("test filter parses")
        .into_expr();
    ODataQuery::from(Some(expr))
}

// ---------------------------------------------------------------------------
// compose_query_with_scope — AND-merges PDP scope but MUST keep filter_hash
// pinned to the user `$filter` (keyset-cursor stability across pages).
// ---------------------------------------------------------------------------

/// A user query whose `filter_hash` mirrors the gateway: it is the hash of the
/// USER `$filter`, not of anything the service later AND-merges in.
fn user_query_with_gateway_hash(filter: &str) -> ODataQuery {
    let mut q = query_with_filter(filter);
    q.filter_hash = toolkit_odata::short_filter_hash(q.filter());
    q
}

#[test]
fn compose_preserves_user_filter_hash_when_scope_narrows() {
    // Regression for the keyset-pagination FILTER_MISMATCH 400: re-hashing the
    // AND-merged filter into `filter_hash` embeds a hash in the next_cursor
    // that the gateway's hash(user $filter) can never match on the follow-up
    // page. `filter_hash` must stay the USER-filter hash.
    let user = user_query_with_gateway_hash("resource_id eq 'r1'");
    let user_hash = user.filter_hash.clone();
    assert!(
        user_hash.is_some(),
        "precondition: gateway populated filter_hash"
    );

    let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
        pep_properties::OWNER_TENANT_ID,
        vec![Uuid::from_u128(0xA)],
    )]));
    let composed = compose_query_with_scope(&user, &scope).expect("compose ok");

    // The filter AST genuinely changed (the tenant predicate was AND-merged):
    // its hash differs from the user-filter hash.
    assert_ne!(
        toolkit_odata::short_filter_hash(composed.filter()),
        user_hash,
        "scope narrowing must change the filter AST",
    );
    // But the stored filter_hash stays pinned to the USER-filter hash, so the
    // keyset cursor's `f` keeps matching the gateway's hash across pages.
    assert_eq!(
        composed.filter_hash, user_hash,
        "compose MUST NOT re-hash the server-injected scope into filter_hash",
    );
}

#[test]
fn compose_unconstrained_scope_is_denied_fail_closed() {
    // An `allow_all` scope on the LIST/aggregate path is a degenerate
    // empty-predicate permit, not a happy-path admin grant. Composition MUST
    // fail closed (via `scope_to_odata_filter`) rather than pass the user
    // filter through unscoped, which would return every tenant's records.
    let user = user_query_with_gateway_hash("resource_id eq 'r1'");
    let err = compose_query_with_scope(&user, &AccessScope::allow_all())
        .expect_err("allow_all -> PermissionDenied");
    assert!(
        matches!(err, UsageCollectorError::PermissionDenied { .. }),
        "allow_all must surface as PermissionDenied, got {err:?}",
    );
}

// ---------------------------------------------------------------------------
// reject_reserved_filter_fields / require_dimensions_declared — Spec §3.11:
// the admissible `$filter` / `group_by` surface is the eight fixed fields
// plus the queried meter's declared metadata keys, recomputed per request.
// ---------------------------------------------------------------------------

fn declared(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_owned()).collect()
}

/// A fixed queried-meter id for tests that don't care which meter, only
/// that `require_dimensions_declared` threads it onto the error.
fn meter_id() -> MeterTypeId {
    const GTS_ID: &str =
        toolkit_gts::gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
    MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
}

/// A bare equality predicate over `field`: `field eq 'x'`.
fn eq_predicate(field: &str) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier(field.to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String("x".to_owned()))),
    )
}

/// Assert `err` is the canonical reserved-filter-field rejection —
/// attributed to `$filter` with `ValidationReason::Validation` — and that
/// its `detail` names the offending field.
fn assert_reserved_field(err: UsageCollectorError, field_name: &str) {
    match err {
        UsageCollectorError::InvalidArgument {
            field,
            reason,
            detail,
            ..
        } => {
            assert_eq!(
                field, "$filter",
                "reserved-field violation attributes to $filter"
            );
            assert_eq!(reason, ValidationReason::Validation);
            assert!(
                detail.contains(field_name),
                "detail must name the offending field '{field_name}': {detail}"
            );
        }
        other => panic!("expected InvalidArgument/Validation, got {other:?}"),
    }
}

/// Assert `err` is the canonical undeclared-metadata-dimension rejection —
/// attributed to `group_by` with `ValidationReason::UnknownMetadataKey`,
/// `resource_name` carrying the queried meter — and that its `detail`
/// names the offending key.
fn assert_undeclared_dimension(err: UsageCollectorError, key: &str) {
    match err {
        UsageCollectorError::InvalidArgument {
            resource_name,
            field,
            reason,
            detail,
            ..
        } => {
            assert_eq!(
                field, "group_by",
                "undeclared-dimension violation attributes to group_by"
            );
            assert_eq!(reason, ValidationReason::UnknownMetadataKey);
            assert_eq!(
                resource_name.as_deref(),
                Some(meter_id().as_str()),
                "resource_name must carry the queried meter, matching \
                 unknown_metadata_key's operator-log shape",
            );
            assert!(
                detail.contains(key),
                "detail must name the offending key '{key}': {detail}"
            );
        }
        other => panic!("expected InvalidArgument/UnknownMetadataKey, got {other:?}"),
    }
}

#[test]
fn a_declared_metadata_key_is_groupable() {
    let dims = [AggregationDimension::Metadata(
        MetadataKey::new("region").unwrap(),
    )];
    require_dimensions_declared(&dims, &declared(&["region", "storage_class"]), &meter_id())
        .expect("a declared key is groupable");
}

#[test]
fn an_undeclared_metadata_key_is_rejected_and_named() {
    let dims = [AggregationDimension::Metadata(
        MetadataKey::new("tier").unwrap(),
    )];
    let err = require_dimensions_declared(&dims, &declared(&["region"]), &meter_id())
        .expect_err("an undeclared key must not be groupable");
    assert!(err.to_string().contains("tier"));
    assert_undeclared_dimension(err, "tier");
}

#[test]
fn the_fixed_dimensions_need_no_declaration() {
    let dims = [
        AggregationDimension::TenantId,
        AggregationDimension::ResourceId,
    ];
    require_dimensions_declared(&dims, &declared(&[]), &meter_id())
        .expect("fixed fields always admissible");
}

#[test]
fn admissibility_is_recomputed_per_request_not_cached() {
    // Same dimension, same call site — only the declared-keys argument
    // differs between the two invocations. A stale-cache bug (an
    // admissible set computed once and reused) would make both calls agree;
    // a correct implementation reads `declared_keys` fresh each time.
    let dims = [AggregationDimension::Metadata(
        MetadataKey::new("region").unwrap(),
    )];

    require_dimensions_declared(&dims, &declared(&[]), &meter_id())
        .expect_err("not yet declared: must be rejected");
    require_dimensions_declared(&dims, &declared(&["region"]), &meter_id())
        .expect("declared a moment later: must now be admissible, without a restart");
    // And the reverse direction: withdrawing the declaration must be
    // honored on the very next call too, not just its addition.
    require_dimensions_declared(&dims, &declared(&[]), &meter_id())
        .expect_err("withdrawn: must be rejected again on the next call");
}

#[test]
fn gts_type_id_is_reserved_as_a_filter_field() {
    // 3.11: it travels as a typed parameter, so any predicate touching it is
    // rejected rather than silently honored.
    let filter = eq_predicate("gts_type_id");
    let err = reject_reserved_filter_fields(&filter)
        .expect_err("gts_type_id must be rejected as a filter field");
    assert_reserved_field(err, "gts_type_id");
}

#[test]
fn the_window_bounds_are_not_filterable() {
    for field in ["window_start", "window_end"] {
        let filter = eq_predicate(field);
        let err = reject_reserved_filter_fields(&filter).expect_err(&format!(
            "{field} must not be filterable: the time range is a first-class parameter"
        ));
        assert_reserved_field(err, field);
    }
}

#[test]
fn a_reserved_field_nested_under_or_is_rejected() {
    // A naive top-level-only check would miss this: the reserved identifier
    // is not itself a top-level conjunct, it is nested under an `or`.
    let filter = ast::Expr::Or(
        Box::new(eq_predicate("resource_id")),
        Box::new(eq_predicate("gts_type_id")),
    );
    let err = reject_reserved_filter_fields(&filter)
        .expect_err("a reserved field nested under `or` must still be rejected");
    assert_reserved_field(err, "gts_type_id");
}

#[test]
fn a_reserved_field_nested_under_not_is_rejected() {
    let filter = ast::Expr::Not(Box::new(eq_predicate("window_start")));
    let err = reject_reserved_filter_fields(&filter)
        .expect_err("a reserved field nested under `not` must still be rejected");
    assert_reserved_field(err, "window_start");
}

#[test]
fn a_reserved_field_nested_in_an_in_list_is_rejected() {
    let filter = ast::Expr::In(
        Box::new(ast::Expr::Identifier("window_end".to_owned())),
        vec![
            ast::Expr::Value(ast::Value::String("a".to_owned())),
            ast::Expr::Value(ast::Value::String("b".to_owned())),
        ],
    );
    let err = reject_reserved_filter_fields(&filter)
        .expect_err("a reserved field named by `in`'s left-hand side must be rejected");
    assert_reserved_field(err, "window_end");
}

#[test]
fn a_reserved_field_nested_in_a_function_argument_is_rejected() {
    // Structurally identical to the `In`-items case above: `Function`'s
    // argument list is the other multi-child AST shape the walk must
    // recurse into rather than treat as opaque.
    let filter = ast::Expr::Function(
        "contains".to_owned(),
        vec![
            ast::Expr::Identifier("window_start".to_owned()),
            ast::Expr::Value(ast::Value::String("x".to_owned())),
        ],
    );
    let err = reject_reserved_filter_fields(&filter)
        .expect_err("a reserved field named inside a function argument must be rejected");
    assert_reserved_field(err, "window_start");
}

#[test]
fn a_non_reserved_filter_is_accepted() {
    let filter = ast::Expr::And(
        Box::new(eq_predicate("resource_id")),
        Box::new(eq_predicate("tenant_id")),
    );
    reject_reserved_filter_fields(&filter).expect("no reserved field named: must be accepted");
}

#[test]
fn reserved_field_match_is_case_insensitive() {
    // The reservation has to be un-evadable, so a case-varied spelling of a
    // reserved field must not walk past it. That reason is local to this
    // check rather than a gear-wide convention — the `$orderby` guards next
    // door (`is_keyset_safe_record_field`, `ensure_tiebreaker`) both match
    // exactly.
    //
    // Load-bearing, not theoretical: `window_start` / `window_end` are
    // filterable-schema fields, so a case-varied spelling of one resolves
    // as a legitimate field and would reach a plugin as a real predicate
    // if this comparison were exact. A case-varied `gts_type_id` has a
    // second net downstream (`toolkit_odata`'s own case-insensitive
    // `UnknownField`, since that name is off the schema); the bounds do
    // not, so this reservation is the only thing between `WINDOW_END` in a
    // `$filter` and the plugin.
    for field in ["GTS_TYPE_ID", "Window_Start", "WINDOW_END"] {
        let filter = eq_predicate(field);
        let err = reject_reserved_filter_fields(&filter).expect_err(&format!(
            "a case-varied spelling of a reserved field ('{field}') must still be rejected"
        ));
        assert_reserved_field(err, field);
    }
}

// ---------------------------------------------------------------------------
// require_metadata_filter_keys_declared — Spec §3.11: `metadata_filter` is
// the dynamic-key side channel `$filter` cannot express a JSON-map key
// predicate through (that's precisely why it exists as a separate
// parameter), so it needs its own declared-keys gate, recomputed per
// request exactly like `group_by`'s.
// ---------------------------------------------------------------------------

/// Build a `MetadataFilter` for `key` with a single candidate value.
fn metadata_filter(key: &str) -> MetadataFilter {
    MetadataFilter::new(key, ["x".to_owned()]).expect("valid metadata filter")
}

/// A [`MetadataFilter`] on `key` over an explicit value set, for the
/// fingerprint tests, where the values are the subject.
fn filter_on(key: &str, values: &[&str]) -> MetadataFilter {
    MetadataFilter::new(key, values.iter().copied()).expect("valid metadata filter")
}

/// Assert `err` is the canonical unknown-metadata-key rejection —
/// `ValidationReason::UnknownMetadataKey`, `resource_name` carrying the
/// queried meter (the same [`UsageCollectorError::unknown_metadata_key`]
/// shape ingestion uses) — and that its `detail` names the offending key.
fn assert_unknown_metadata_key(err: UsageCollectorError, key: &str) {
    match err {
        UsageCollectorError::InvalidArgument {
            resource_name,
            reason,
            detail,
            ..
        } => {
            assert_eq!(reason, ValidationReason::UnknownMetadataKey);
            assert_eq!(
                resource_name.as_deref(),
                Some(meter_id().as_str()),
                "resource_name must carry the queried meter",
            );
            assert!(
                detail.contains(key),
                "detail must name the offending key '{key}': {detail}"
            );
        }
        other => panic!("expected InvalidArgument/UnknownMetadataKey, got {other:?}"),
    }
}

#[test]
fn a_declared_metadata_filter_key_is_accepted() {
    let filters = [metadata_filter("region")];
    require_metadata_filter_keys_declared(
        &filters,
        &declared(&["region", "storage_class"]),
        &meter_id(),
    )
    .expect("a declared key is admissible");
}

#[test]
fn an_undeclared_metadata_filter_key_is_rejected_and_named() {
    let filters = [metadata_filter("tier")];
    let err = require_metadata_filter_keys_declared(&filters, &declared(&["region"]), &meter_id())
        .expect_err("an undeclared key must not be admissible");
    assert_unknown_metadata_key(err, "tier");
}

#[test]
fn metadata_filter_admissibility_is_recomputed_per_request() {
    // Same key, same call site — only the declared-keys argument differs
    // between invocations, exactly the `group_by` proof mirrored onto
    // `metadata_filter`: nothing about admissibility is cached at the
    // wrong layer.
    let filters = [metadata_filter("region")];

    require_metadata_filter_keys_declared(&filters, &declared(&[]), &meter_id())
        .expect_err("not yet declared: must be rejected");
    require_metadata_filter_keys_declared(&filters, &declared(&["region"]), &meter_id())
        .expect("declared a moment later: must now be admissible, without a restart");
    require_metadata_filter_keys_declared(&filters, &declared(&[]), &meter_id())
        .expect_err("withdrawn: must be rejected again on the next call");
}

// ---------------------------------------------------------------------------
// establish_keyset_order — the first-page mode of the keyset floor.
//
// DESIGN §3.1 "Order admissibility" allocates this to the Query Gateway so
// the plugin *always* receives a gap-free, uniform-direction, never-null
// keyset. It lives in the domain rather than in the REST handler for that
// reason: an in-process caller reaches `Service::list_usage_records`
// directly with an `ODataQuery` of their own construction, and a
// normalization at the REST edge alone left the SPI's order slot empty on
// exactly that surface.
// ---------------------------------------------------------------------------

/// The order `query` carries, as `(field, direction)` pairs.
fn order_of(query: &ODataQuery) -> Vec<(String, SortDir)> {
    query
        .order
        .0
        .iter()
        .map(|key| (key.field.clone(), key.dir))
        .collect()
}

/// An [`ODataQuery`] whose order is exactly `keys`.
fn query_ordered_by(keys: &[(&str, SortDir)]) -> ODataQuery {
    let mut query = ODataQuery::new();
    query.order = ODataOrderBy(
        keys.iter()
            .map(|(field, dir)| OrderKey {
                field: (*field).to_owned(),
                dir: *dir,
            })
            .collect(),
    );
    query
}

/// Assert `query`'s order is exactly `expected`.
fn assert_order(query: &ODataQuery, expected: &[(&str, SortDir)]) {
    let actual = order_of(query);
    let expected: Vec<(String, SortDir)> = expected
        .iter()
        .map(|(f, d)| ((*f).to_owned(), *d))
        .collect();
    assert_eq!(actual, expected, "effective keyset order");
}

/// Assert `query.order` satisfies every property the Plugin SPI documents:
/// non-empty, one sort direction throughout, only never-null keys, and
/// both canonical keyset fields named.
///
/// Position is deliberately NOT asserted. The guarantee is membership —
/// `ensure_tiebreaker` appends only what is missing, so a caller order
/// already naming `id` keeps it leading — and asserting a shape here would
/// re-import the positional claim these tests exist to disprove.
///
/// It follows that this helper is USELESS behind an exact-shape assertion:
/// `assert_order` pins every key and direction, which implies all four
/// properties, so a call after one can never fail. It is only called from
/// the shape-free table-driven tests, where the input shapes outnumber the
/// exact shapes anyone would write out and it is the only thing that can
/// fail. Keeping it is worth it anyway: it is the SPI contract as
/// executable code, and slice 6's plugin contract suite starts here.
///
/// The two field names are spelled out rather than read from
/// [`CANONICAL_KEYSET_FIELDS`] because an assertion that reads the
/// implementation's own constant tests nothing.
/// `the_canonical_keyset_is_the_covered_period_end_and_the_record_id`
/// anchors the constant against the same literals, closing the drift
/// direction this duplication leaves open.
fn assert_keyset_guarantee(query: &ODataQuery) {
    let keys = &query.order.0;
    assert!(!keys.is_empty(), "the keyset must not be empty");
    let dir = keys[0].dir;
    assert!(
        keys.iter().all(|key| key.dir == dir),
        "the keyset must use one sort direction: {:?}",
        order_of(query),
    );
    for key in keys {
        assert!(
            is_keyset_safe_record_field(&key.field),
            "keyset key `{}` is not a never-null record attribute",
            key.field,
        );
    }
    for field in ["window_end", "id"] {
        assert!(
            keys.iter().any(|key| key.field == field),
            "the keyset must name `{field}`, so the sort tuple is unique: {:?}",
            order_of(query),
        );
    }
}

/// Assert `err` is the canonical `$orderby` rejection.
fn assert_orderby_rejection(err: UsageCollectorError) {
    match err {
        UsageCollectorError::InvalidArgument { field, reason, .. } => {
            assert_eq!(field, "$orderby", "the rejection must blame the order");
            assert_eq!(reason, ValidationReason::Validation);
        }
        other => panic!("expected InvalidArgument on $orderby, got {other:?}"),
    }
}

#[test]
fn the_canonical_keyset_is_the_covered_period_end_and_the_record_id() {
    // The anchor for every literal `"window_end"` / `"id"` in this file,
    // and for `assert_keyset_guarantee`'s in particular. Those are spelled
    // out rather than read from the constant, which is what lets them
    // catch a repoint — but it also means the constant could gain a third
    // field, or lose one, with no assertion noticing. This is that
    // assertion, and the only place the two spellings meet.
    assert_eq!(CANONICAL_KEYSET_FIELDS, ["window_end", "id"]);
}

#[test]
fn an_absent_orderby_normalizes_to_the_canonical_keyset() {
    // `(window_end asc, id asc)`: the column the range selects on is the
    // column the page orders by, so one index serves both, and `id` closes
    // the run of rows that share a `window_end`.
    let mut query = ODataQuery::new();
    establish_keyset_order(&mut query).expect("an empty order is floorable");
    assert_order(
        &query,
        &[("window_end", SortDir::Asc), ("id", SortDir::Asc)],
    );
}

#[test]
fn a_descending_caller_order_gains_the_suffix_in_its_own_direction() {
    // Row-value keyset comparison needs a uniform direction; pairing a
    // descending caller order with an ascending suffix cannot compose.
    let mut query = query_ordered_by(&[("resource_id", SortDir::Desc)]);
    establish_keyset_order(&mut query).expect("a uniform desc order is floorable");
    assert_order(
        &query,
        &[
            ("resource_id", SortDir::Desc),
            ("window_end", SortDir::Desc),
            ("id", SortDir::Desc),
        ],
    );
}

#[test]
fn an_order_on_the_covered_period_end_gains_only_the_missing_tiebreaker() {
    // `$orderby=window_end` already names the leading suffix key, so only
    // `id` is appended — `ensure_tiebreaker` skips a field the order names.
    let mut query = query_ordered_by(&[("window_end", SortDir::Asc)]);
    establish_keyset_order(&mut query).expect("ordering on the period end is admissible");
    assert_order(
        &query,
        &[("window_end", SortDir::Asc), ("id", SortDir::Asc)],
    );
}

// The three cases below are the shape the guarantee is NOT: a caller order
// that already names `id` keeps it where it is, so the floored order does
// not end in `(window_end, id)`. Every other happy-path test here starts
// from an order naming neither canonical field, which is why the whole
// suite — and fifteen mutations of it — was blind to this until a spec
// review read the claim literally.

#[test]
fn an_order_already_naming_the_tiebreaker_gains_only_the_period_end() {
    // `$orderby=id`. `id` is on the filterable schema and keyset-safe, so
    // this is reachable on both surfaces. `ensure_tiebreaker` skips a
    // field the order names and appends the missing one at the end, so the
    // result leads on `id` — and is still globally unique, which is the
    // property that actually matters.
    let mut query = query_ordered_by(&[("id", SortDir::Asc)]);
    establish_keyset_order(&mut query).expect("ordering on id is admissible");
    assert_order(
        &query,
        &[("id", SortDir::Asc), ("window_end", SortDir::Asc)],
    );
}

#[test]
fn an_order_naming_both_canonical_fields_is_left_exactly_as_it_came() {
    // `$orderby=id,window_end` — both names present, in the caller's own
    // sequence. Nothing is appended and nothing is reordered: the floor
    // adds names, it never rearranges them.
    let mut query = query_ordered_by(&[("id", SortDir::Desc), ("window_end", SortDir::Desc)]);
    establish_keyset_order(&mut query).expect("both canonical fields named");
    assert_order(
        &query,
        &[("id", SortDir::Desc), ("window_end", SortDir::Desc)],
    );
}

#[test]
fn a_caller_tiebreaker_in_the_middle_keeps_its_position() {
    // `$orderby=tenant_id,id` floors to `(tenant_id, id, window_end)`:
    // `window_end` lands *after* the tiebreaker, so neither canonical
    // field is last and the sort tuple is unique regardless.
    let mut query = query_ordered_by(&[("tenant_id", SortDir::Asc), ("id", SortDir::Asc)]);
    establish_keyset_order(&mut query).expect("a mandatory-key order is admissible");
    assert_order(
        &query,
        &[
            ("tenant_id", SortDir::Asc),
            ("id", SortDir::Asc),
            ("window_end", SortDir::Asc),
        ],
    );
}

#[test]
fn an_order_on_created_at_is_rejected() {
    // `created_at` is not a record attribute any more. The floor's
    // classification is a fail-closed allowlist, so a stale order key is
    // refused rather than resolving to the covered period or to nothing.
    let mut query = query_ordered_by(&[("created_at", SortDir::Asc)]);
    let err = establish_keyset_order(&mut query).expect_err("a retired order key must fail closed");
    assert_orderby_rejection(err);
}

#[test]
fn an_order_on_a_domain_optional_attribute_is_rejected() {
    // A row-value tuple whose leading column is NULL compares as NULL, so
    // every NULL-keyed row would silently drop out of the page.
    for field in ["subject_id", "subject_type", "corrects_id"] {
        let mut query = query_ordered_by(&[(field, SortDir::Asc)]);
        let err = establish_keyset_order(&mut query)
            .expect_err(&format!("ordering on optional `{field}` must be refused"));
        assert_orderby_rejection(err);
    }
}

#[test]
fn a_mixed_direction_order_is_rejected() {
    // Appending a suffix cannot repair this: no direction makes a
    // mixed-direction tuple comparison meaningful.
    let mut query =
        query_ordered_by(&[("resource_id", SortDir::Asc), ("tenant_id", SortDir::Desc)]);
    let err =
        establish_keyset_order(&mut query).expect_err("a mixed-direction order must be refused");
    assert_orderby_rejection(err);
}

#[test]
fn a_uniform_multi_key_order_on_mandatory_attributes_is_accepted() {
    // Guards the two rejections against over-rejecting: a uniform
    // multi-key order over mandatory attributes is a sound keyset and must
    // survive, gaining both canonical fields (it names neither) in its own
    // direction.
    let mut query = query_ordered_by(&[
        ("tenant_id", SortDir::Desc),
        ("resource_type", SortDir::Desc),
    ]);
    establish_keyset_order(&mut query).expect("a uniform mandatory-key order is sound");
    assert_order(
        &query,
        &[
            ("tenant_id", SortDir::Desc),
            ("resource_type", SortDir::Desc),
            ("window_end", SortDir::Desc),
            ("id", SortDir::Desc),
        ],
    );
}

/// Every admissible first-page input shape, as `(field, direction)` pairs.
///
/// Deliberately wider than the exact-shape tests above: orders naming
/// neither / one / both canonical fields, in leading, middle and trailing
/// positions, at one to three caller keys, in both directions. Only
/// durable mandatory attributes appear — `status` leaves the filterable
/// schema in a later slice.
const ADMISSIBLE_ORDER_SHAPES: &[&[(&str, SortDir)]] = &[
    &[],
    &[("window_end", SortDir::Asc)],
    &[("window_end", SortDir::Desc)],
    &[("id", SortDir::Asc)],
    &[("id", SortDir::Desc)],
    &[("id", SortDir::Asc), ("window_end", SortDir::Asc)],
    &[("window_end", SortDir::Desc), ("id", SortDir::Desc)],
    &[("tenant_id", SortDir::Asc)],
    &[("resource_id", SortDir::Desc)],
    &[("window_start", SortDir::Asc)],
    &[("tenant_id", SortDir::Asc), ("id", SortDir::Asc)],
    &[
        ("resource_type", SortDir::Desc),
        ("window_end", SortDir::Desc),
    ],
    &[
        ("resource_id", SortDir::Asc),
        ("resource_type", SortDir::Asc),
    ],
    &[
        ("resource_id", SortDir::Desc),
        ("resource_type", SortDir::Desc),
        ("tenant_id", SortDir::Desc),
    ],
    &[
        ("tenant_id", SortDir::Asc),
        ("id", SortDir::Asc),
        ("window_end", SortDir::Asc),
    ],
];

#[test]
fn the_floor_establishes_the_documented_guarantee_for_every_admissible_shape() {
    // The guarantee, and ONLY the guarantee: no exact-shape assertion
    // anywhere in this test, so `assert_keyset_guarantee` is the single
    // thing that can fail. The shape tests above cover four inputs between
    // them; this covers every admissible shape the floor can be handed,
    // which is what catches a mutation that happens to be correct on the
    // four written-out ones.
    for keys in ADMISSIBLE_ORDER_SHAPES {
        let mut query = query_ordered_by(keys);
        establish_keyset_order(&mut query)
            .unwrap_or_else(|e| panic!("{keys:?} is an admissible order, but: {e:?}"));
        assert_keyset_guarantee(&query);
    }
}

#[test]
fn the_floor_is_idempotent_for_every_admissible_shape() {
    // Idempotence is what makes a service-level floor safe on the REST
    // path, where the handler has already applied it: a second
    // application must be a no-op, not a second append. Across the whole
    // table, because every shape passes through both call sites — not
    // just the empty order.
    for keys in ADMISSIBLE_ORDER_SHAPES {
        let mut query = query_ordered_by(keys);
        establish_keyset_order(&mut query).expect("first application");
        let once = order_of(&query);
        establish_keyset_order(&mut query).expect("second application");
        assert_eq!(
            order_of(&query),
            once,
            "re-flooring {keys:?} must not change it",
        );
    }
}

#[test]
fn the_floor_touches_nothing_but_the_order() {
    // The floor runs after PDP composition, so it must not disturb the
    // composed `$filter`, the `filter_hash` the cursor is validated
    // against, or the page size.
    let mut query = query_with_filter("resource_id eq 'r1'");
    query.filter_hash = Some("h0".to_owned());
    query.limit = Some(7);
    let filter_before = format!("{:?}", query.filter);

    establish_keyset_order(&mut query).expect("floorable");

    assert_eq!(
        format!("{:?}", query.filter),
        filter_before,
        "$filter must be untouched",
    );
    assert_eq!(query.filter_hash.as_deref(), Some("h0"));
    assert_eq!(query.limit, Some(7));
}

// ---------------------------------------------------------------------------
// require_continuation_keyset — the continuation mode of the keyset floor.
//
// An order that arrived reconstructed from a token is checked, never
// extended. The token's boundary values (`CursorV1::k`) line up one for one
// with the keys it was minted under, so appending a key would widen the sort
// tuple past the values available to compare against and hand the plugin a
// misaligned continuation — a silently wrong page, where refusing is merely
// a refused one. Nothing downstream would catch it:
// `validate_cursor_against` never checks the token's width against the
// order's, and the gear hands it no filter hash at all.
//
// That the order comes back UNCHANGED is not asserted here, because
// `require_continuation_keyset` takes `&ODataQuery` and the compiler
// enforces it. What a test can still get wrong is the service calling the
// wrong mode, so the no-op lives in `read_path_keyset_floor_tests`, where
// the plugin can be asked what it actually received.
// ---------------------------------------------------------------------------

/// Mint a continuation the way a conforming plugin does, then decode it the
/// way the handler does.
///
/// The round trip is the point: `to_signed_tokens` is what a plugin puts in
/// `next_cursor.s`, and `ODataOrderBy::from_signed_tokens` is literally
/// what `prepare_list_query` step 3 calls to rebuild `query.order` from it.
/// Deriving the order FROM the token — rather than setting both from the
/// same list — is what makes these fixtures reproduce the path they model
/// instead of being self-consistent by construction.
fn continuation_bound_to(order: &ODataOrderBy) -> ODataQuery {
    let signed = order.to_signed_tokens();
    let mut query = ODataQuery::new();
    query.order = ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
    query.cursor = Some(CursorV1 {
        // One boundary value per key of the order the token was minted
        // under — the alignment the refusal exists to preserve.
        k: query
            .order
            .0
            .iter()
            .map(|_| "boundary".to_owned())
            .collect(),
        o: query.order.0[0].dir,
        s: signed,
        f: None,
        d: "fwd".to_owned(),
    });
    query
}

/// A continuation bound to `keys`, as a conforming plugin would have minted
/// it had it been handed that order.
fn continuation_ordered_by(keys: &[(&str, SortDir)]) -> ODataQuery {
    continuation_bound_to(&query_ordered_by(keys).order)
}

/// Assert `err` blames `cursor` — the parameter a continuation request
/// actually carries — with the wire code the contract enumerates for a
/// refused token. Blaming `$orderby` here would name a parameter the
/// caller cannot send alongside a cursor.
fn assert_cursor_rejection(err: UsageCollectorError) {
    match err {
        UsageCollectorError::InvalidArgument { field, reason, .. } => {
            assert_eq!(
                field, "cursor",
                "a continuation defect must blame the token"
            );
            assert_eq!(reason, ValidationReason::InvalidCursor);
        }
        other => panic!("expected InvalidArgument on cursor, got {other:?}"),
    }
}

#[test]
fn every_order_the_floor_establishes_is_accepted_as_a_continuation() {
    // The two modes MUST agree, or pagination stops after page one:
    // whatever `establish_keyset_order` hands the plugin comes back as a
    // token, and `require_continuation_keyset` has to accept the order
    // decoded from it. Running the whole admissible table through both in
    // sequence is what pins that, and it is also the second place
    // `assert_keyset_guarantee` can fail — the acceptance criterion and
    // the documented guarantee are meant to be the same predicate.
    for keys in ADMISSIBLE_ORDER_SHAPES {
        let mut first_page = query_ordered_by(keys);
        establish_keyset_order(&mut first_page)
            .unwrap_or_else(|e| panic!("{keys:?} is an admissible order, but: {e:?}"));
        assert_keyset_guarantee(&first_page);

        let follow_up = continuation_bound_to(&first_page.order);
        require_continuation_keyset(&follow_up).unwrap_or_else(|e| {
            panic!("a token minted from the floored form of {keys:?} must be accepted, but: {e:?}")
        });
        assert_keyset_guarantee(&follow_up);
        assert_eq!(
            order_of(&follow_up),
            order_of(&first_page),
            "the token round trip must preserve {keys:?}'s floored order",
        );
        assert_eq!(
            follow_up.cursor.as_ref().expect("cursor").k.len(),
            follow_up.order.0.len(),
            "one boundary value per key: the alignment the refusal protects",
        );
    }
}

#[test]
fn a_continuation_missing_a_canonical_field_is_refused_not_extended() {
    // The case that would otherwise misalign: appending `window_end` here
    // would leave a two-key order against the token's one boundary value.
    let query = continuation_ordered_by(&[("resource_id", SortDir::Asc)]);
    let err = require_continuation_keyset(&query)
        .expect_err("a token bound to a non-unique keyset must be refused");
    assert_cursor_rejection(err);
}

#[test]
fn a_continuation_missing_only_the_tiebreaker_is_refused() {
    // `+window_end` alone: unique-enough-looking, but a page boundary
    // inside a run of equal `window_end`s drops the rest of the run.
    let query = continuation_ordered_by(&[("window_end", SortDir::Asc)]);
    let err = require_continuation_keyset(&query)
        .expect_err("a token missing the tiebreaker must be refused");
    assert_cursor_rejection(err);
}

#[test]
fn a_continuation_missing_only_the_period_end_is_refused() {
    // The mirror case, and the one a positional reading would miss:
    // `+id` alone is globally unique, so it looks like a sound keyset —
    // but the plugin's `ORDER BY` then has no time key and the page order
    // stops agreeing with the column the range selects on.
    let query = continuation_ordered_by(&[("id", SortDir::Asc)]);
    let err = require_continuation_keyset(&query)
        .expect_err("a token missing the period end must be refused");
    assert_cursor_rejection(err);
}

#[test]
fn an_empty_continuation_order_is_refused_rather_than_defaulted() {
    // An in-process caller can set a cursor with no order at all — the
    // handler cannot, since `from_signed_tokens` rejects empty tokens,
    // which is why this fixture is built by hand rather than round-tripped.
    // The first-page mode would default it to the canonical keyset; here
    // that would invent a keyset the token was never minted under.
    let mut query = ODataQuery::new();
    query.cursor = Some(CursorV1 {
        k: vec!["boundary".to_owned()],
        o: SortDir::Asc,
        s: "+window_end,+id".to_owned(),
        f: None,
        d: "fwd".to_owned(),
    });
    let err = require_continuation_keyset(&query)
        .expect_err("an empty continuation order must be refused");
    assert_cursor_rejection(err);
}

#[test]
fn a_continuation_naming_a_retired_key_is_refused_as_a_cursor_defect() {
    // `+created_at,+id` — a token minted before the covered period landed.
    // Refused either way; the point is the attribution, which must be the
    // token rather than an `$orderby` the caller never sent.
    let query = continuation_ordered_by(&[("created_at", SortDir::Asc), ("id", SortDir::Asc)]);
    let err = require_continuation_keyset(&query)
        .expect_err("a token bound to a retired key must be refused");
    assert_cursor_rejection(err);
}

#[test]
fn a_mixed_direction_continuation_is_refused_as_a_cursor_defect() {
    let query = continuation_ordered_by(&[("window_end", SortDir::Asc), ("id", SortDir::Desc)]);
    let err = require_continuation_keyset(&query)
        .expect_err("a token bound to a mixed-direction keyset must be refused");
    assert_cursor_rejection(err);
}

// ---------------------------------------------------------------------------
// read_fingerprint / require_cursor_fingerprint — the query a continuation is
// bound to.
//
// `CursorV1::f` exists so a caller who changes their query between pages is
// refused rather than served a continuation minted over a different row set.
// While the mandatory window lived inside `$filter`, `short_filter_hash`
// covered it for free; it is a typed parameter now, so both halves have to be
// pinned here — the range being present is the point of the change, and the
// filter still being present is what says the range did not quietly replace
// the property it was added to.
// ---------------------------------------------------------------------------

/// [`read_fingerprint`] over a fixed meter and no metadata filter, for the
/// tests whose subject is the `$filter` or the range. The meter and the
/// metadata filter have their own tests below.
fn fingerprint(query: &ODataQuery, range: usage_collector_sdk::TimeRange) -> String {
    read_fingerprint(&meter_id(), range, query, &[])
}

/// A range starting at `secs` past the epoch and running one hour.
fn hour_from(secs: i64) -> usage_collector_sdk::TimeRange {
    let from = time::OffsetDateTime::from_unix_timestamp(secs).expect("in-range timestamp");
    usage_collector_sdk::TimeRange::new(from, from + time::Duration::hours(1)).expect("range")
}

#[test]
fn the_fingerprint_covers_both_the_filter_and_the_range() {
    let range_a = hour_from(1_700_000_000);
    let range_b = hour_from(1_800_000_000);
    let plain = ODataQuery::default();
    let filtered = query_with_filter("resource_id eq 'r1'");

    assert_ne!(
        fingerprint(&plain, range_a),
        fingerprint(&plain, range_b),
        "the range must move the fingerprint",
    );
    assert_ne!(
        fingerprint(&filtered, range_a),
        fingerprint(&plain, range_a),
        "the filter must move the fingerprint",
    );
    assert_eq!(
        fingerprint(&filtered, range_a),
        fingerprint(&filtered, range_a),
        "and it must be stable, or no cursor would ever validate",
    );
}

#[test]
fn the_fingerprint_of_a_filterless_query_still_carries_the_range() {
    // `short_filter_hash` returns `None` for an absent filter, and an
    // absent filter is a legitimate complete request (the PDP scope alone
    // narrows it). A rendering that short-circuited on the `None` would
    // drop the range for exactly the callers who send no `$filter`.
    let plain = ODataQuery::default();
    assert!(
        fingerprint(&plain, hour_from(1_700_000_000))
            .contains(&hour_from(1_700_000_000).canonical_form()),
        "a filterless query's fingerprint must still carry the range",
    );
}

#[test]
fn the_fingerprint_separates_ranges_differing_only_below_the_microsecond() {
    // The truncation hole, at the level that matters: if the range
    // rendering truncated to the identity derivation's fixed
    // six-digit-microsecond form, these two ranges would fingerprint
    // identically and a cursor minted under either would validate against
    // the other.
    let from = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp");
    let coarse =
        usage_collector_sdk::TimeRange::new(from, from + time::Duration::hours(1)).expect("range");
    let nudged = usage_collector_sdk::TimeRange::new(
        from,
        from + time::Duration::hours(1) + time::Duration::nanoseconds(1),
    )
    .expect("range");
    let query = query_with_filter("resource_id eq 'r1'");

    assert_ne!(fingerprint(&query, coarse), fingerprint(&query, nudged),);
}

#[test]
fn a_cursor_carrying_the_same_fingerprint_is_accepted() {
    let bound = fingerprint(&ODataQuery::default(), hour_from(1_700_000_000));
    let mut query = continuation_ordered_by(&[("window_end", SortDir::Asc), ("id", SortDir::Asc)]);
    query.cursor.as_mut().expect("cursor").f = Some(bound.clone());

    require_cursor_fingerprint(query.cursor.as_ref().expect("cursor"), &bound)
        .expect("a cursor minted over this very query must be accepted");
}

#[test]
fn a_cursor_carrying_a_different_fingerprint_is_refused_as_a_query_mismatch() {
    let query = continuation_ordered_by(&[("window_end", SortDir::Asc), ("id", SortDir::Asc)]);
    let mut cursor = query.cursor.expect("cursor");
    cursor.f = Some(fingerprint(
        &ODataQuery::default(),
        hour_from(1_800_000_000),
    ));

    let err = require_cursor_fingerprint(
        &cursor,
        &fingerprint(&ODataQuery::default(), hour_from(1_700_000_000)),
    )
    .expect_err("a cursor minted over another query must be refused");
    assert_query_mismatch_rejection(err);
}

#[test]
fn a_cursor_carrying_no_fingerprint_at_all_is_refused() {
    // `validate_cursor_against` skips its own comparison whenever either
    // side is absent, which is the hole this check exists to close: the
    // cursor is caller-supplied JSON, so "no fingerprint recorded" is not
    // evidence of a matching query. A conforming plugin always has one to
    // mint, because the read path assigns it onto every dispatch.
    let query = continuation_ordered_by(&[("window_end", SortDir::Asc), ("id", SortDir::Asc)]);
    let cursor = query.cursor.expect("cursor");
    assert!(
        cursor.f.is_none(),
        "precondition: this fixture mints no fingerprint",
    );

    let err = require_cursor_fingerprint(
        &cursor,
        &fingerprint(&ODataQuery::default(), hour_from(1_700_000_000)),
    )
    .expect_err("an unbound cursor must be refused rather than admitted");
    assert_query_mismatch_rejection(err);
}

/// Assert `err` blames `cursor` with the code the contract enumerates for
/// a token minted over a different query. `FILTER_MISMATCH` rather than
/// `INVALID_CURSOR`: the token is structurally fine, it just belongs to
/// another query — and a changed range is a changed query from the
/// caller's side.
fn assert_query_mismatch_rejection(err: UsageCollectorError) {
    match err {
        UsageCollectorError::InvalidArgument { field, reason, .. } => {
            assert_eq!(field, "cursor");
            assert_eq!(reason, ValidationReason::FilterMismatch);
        }
        other => panic!("expected InvalidArgument on cursor, got {other:?}"),
    }
}

#[test]
fn the_fingerprint_covers_the_meter() {
    // `gts_type_id` is a typed parameter, so no filter hash has ever
    // covered it. Without it here a caller can continue a cursor against
    // another meter entirely and be served rows from a table the token
    // knows nothing about — the widest version of the wrong-page failure,
    // and still a `200`.
    let range = hour_from(1_700_000_000);
    let other = MeterTypeId::new(toolkit_gts::gts_id!(
        "cf.core.uc.usage_record.v1~cf.mini_chat._.other_meter.v1~"
    ))
    .expect("valid gts_type_id");
    assert_ne!(meter_id(), other, "precondition: two different meters");

    assert_ne!(
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), &[]),
        read_fingerprint(&other, range, &ODataQuery::default(), &[]),
    );
}

#[test]
fn the_fingerprint_covers_the_metadata_filter() {
    // The other typed row-selector, and the one the `toolkit-odata` grammar
    // cannot express at all — so it never travels through `$filter` and no
    // filter hash can reach it.
    let range = hour_from(1_700_000_000);
    let plain: &[MetadataFilter] = &[];
    let region_eu = [filter_on("region", &["eu"])];
    let region_us = [filter_on("region", &["us"])];

    assert_ne!(
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), plain),
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), &region_eu),
        "adding a metadata filter must move the fingerprint",
    );
    assert_ne!(
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), &region_eu),
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), &region_us),
        "and so must changing one of its values",
    );
}

#[test]
fn the_fingerprint_ignores_how_a_metadata_filter_was_spelled() {
    // The over-rejection half, and it is not theoretical: a REST caller's
    // repeated `metadata.<key>` parameters reach `MetadataFilter::values`
    // in query-string order with duplicates intact, and an in-process
    // caller can hand over the entries in any order. All four spellings
    // below are the same query — OR within a key, AND across keys — so a
    // rendering that read the slice verbatim would refuse a perfectly
    // valid page two for a caller who merely re-ordered their own query
    // string.
    let range = hour_from(1_700_000_000);
    let canonical = [
        filter_on("region", &["eu", "us"]),
        filter_on("tier", &["gold"]),
    ];
    let equivalents = [
        // Entries in the other order.
        vec![
            filter_on("tier", &["gold"]),
            filter_on("region", &["eu", "us"]),
        ],
        // Values in the other order.
        vec![
            filter_on("region", &["us", "eu"]),
            filter_on("tier", &["gold"]),
        ],
        // A repeated value: `?metadata.region=eu&metadata.region=eu&…`.
        vec![
            filter_on("region", &["eu", "us", "eu"]),
            filter_on("tier", &["gold"]),
        ],
    ];

    let expected = read_fingerprint(&meter_id(), range, &ODataQuery::default(), &canonical);
    for (i, spelling) in equivalents.iter().enumerate() {
        assert_eq!(
            read_fingerprint(&meter_id(), range, &ODataQuery::default(), spelling),
            expected,
            "spelling {i} means the same query and MUST fingerprint the same",
        );
    }
}

#[test]
fn the_fingerprint_separates_two_filters_on_one_key_from_one_merged_filter() {
    // Normalization must not go so far as to merge same-key entries:
    // `region in {eu} AND region in {us}` selects nothing, while
    // `region in {eu, us}` selects both. Two different queries, so two
    // different fingerprints — a merge here would let a cursor minted
    // under the second be served against the first.
    let range = hour_from(1_700_000_000);
    let two = [filter_on("region", &["eu"]), filter_on("region", &["us"])];
    let merged = [filter_on("region", &["eu", "us"])];

    assert_ne!(
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), &two),
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), &merged),
    );
}

#[test]
fn the_fingerprint_is_injective_across_field_boundaries() {
    // Why the rendering is length-prefixed rather than separator-joined.
    // A metadata key is domain-opaque — `MetadataKey::new` rejects only the
    // empty string and NUL — and a value is unconstrained, so a field's
    // content can contain whatever separator the rendering picks. A field
    // whose content imitates a boundary makes two different queries render
    // alike, and a cursor minted under one then validates against the
    // other.
    //
    // The first pair is a REAL collision under `field + sep` joining, and
    // finding it meant constructing against that mutant rather than
    // guessing: the `("a", ["bc"])` / `("ab", ["c"])` pair further down
    // does NOT collide, because the rendering interleaves a value count
    // between key and value and the two counts disagree. A separator-join
    // mutation survived the whole suite until this pair existed. With `~`
    // as the separator both sides here render `…~x~2~1~a~`: on the left the
    // key `x~2` splits into exactly the two tokens the right side builds
    // from a key and its value count.
    let range = hour_from(1_700_000_000);
    let fp = |metadata: &[MetadataFilter]| {
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), metadata)
    };

    for (left, right) in [
        // Collides under a `~` join — the GTS terminator, and
        // `canonical_form`'s own join between its two bounds.
        (
            vec![filter_on("x~2", &["a"])],
            vec![filter_on("x", &["1", "a"])],
        ),
        // The same construction against a `:` join, the character the
        // length prefix itself uses, so a "just swap the delimiter" edit
        // is covered too.
        (
            vec![filter_on("x:2", &["a"])],
            vec![filter_on("x", &["1", "a"])],
        ),
        // Different queries whether or not a given join happens to
        // collide on them.
        (vec![filter_on("a", &["bc"])], vec![filter_on("ab", &["c"])]),
        (
            vec![filter_on("k", &["a~b"])],
            vec![filter_on("k~a", &["b"])],
        ),
    ] {
        assert_ne!(
            fp(&left),
            fp(&right),
            "{left:?} and {right:?} are different queries",
        );
    }
}
