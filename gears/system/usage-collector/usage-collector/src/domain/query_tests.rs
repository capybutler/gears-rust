//! Unit tests for [`compose_query_with_scope`] and for
//! [`reject_reserved_filter_fields`] / [`require_dimensions_declared`] /
//! [`require_metadata_filter_keys_declared`], the Spec §3.11 gate on the
//! admissible `$filter` / `group_by` / `metadata_filter` surface.
//!
//! There is no bounded-window guard to test: the mandatory read range is a
//! typed [`usage_collector_sdk::TimeRange`] parameter on both read paths
//! rather than a `$filter` conjunct, so nothing about it can be absent,
//! one-sided, or hidden under an `or`.

use std::collections::BTreeSet;

use toolkit_odata::{ODataQuery, ast};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use usage_collector_sdk::{
    AggregationDimension, MetadataFilter, MetadataKey, MeterTypeId, UsageCollectorError,
    ValidationReason,
};
use uuid::Uuid;

use super::{
    compose_query_with_scope, reject_reserved_filter_fields, require_dimensions_declared,
    require_metadata_filter_keys_declared,
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
    // Not exploitable today (`window_start` /
    // `window_end` are not yet
    // filterable-schema fields, and a case-varied `gts_type_id` would dead-
    // end as `toolkit_odata`'s own case-insensitive `UnknownField`
    // downstream) — but `window_start` / `window_end` become real
    // filterable fields in the record-model slice after this plan, at which
    // point a case-varied spelling must still hit this reservation rather
    // than resolving as a legitimate field.
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
