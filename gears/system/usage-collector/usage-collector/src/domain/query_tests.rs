//! Unit tests for [`compose_query_with_scope`], for
//! [`reject_unpublished_filter_fields`] / [`require_dimensions_declared`] /
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

use std::collections::{BTreeMap, BTreeSet};

use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use usage_collector_sdk::{
    AggregationDimension, MetadataFilter, MetadataKey, MeterTypeId, UsageCollectorError,
    ValidationReason, is_keyset_safe_record_field,
};
use uuid::Uuid;

use super::{
    CANONICAL_KEYSET_FIELDS, PUBLISHED_FILTER_FIELDS, compose_query_with_scope,
    establish_keyset_order, read_fingerprint, read_fingerprint_pre_image,
    reject_off_label_literals, reject_unpublished_filter_fields, require_continuation_keyset,
    require_cursor_fingerprint, require_dimensions_declared, require_metadata_filter_keys_declared,
    require_metadata_filter_within_caps,
};

/// Build an [`ODataQuery`] whose `$filter` is the parsed `filter` string.
fn query_with_filter(filter: &str) -> ODataQuery {
    let expr = toolkit_odata::parse_filter_string(filter)
        .expect("test filter parses")
        .into_expr();
    ODataQuery::from(Some(expr))
}

// compose_query_with_scope — AND-merges PDP scope but MUST keep filter_hash
// pinned to the user `$filter` (keyset-cursor stability across pages).

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

// reject_unpublished_filter_fields / require_dimensions_declared — Spec §3.11:
// the admissible `$filter` / `group_by` surface is the fixed published
// fields plus the queried meter's declared metadata keys, recomputed per
// request.

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

/// `<field> eq '<value>'`, with the literal under the caller's control.
///
/// `eq_predicate` fixes the value at `"x"`, which suits the
/// field-admissibility guards next door but not the closed-label checks,
/// whose subject is the literal.
fn eq_str(field: &str, value: &str) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier(field.to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String(value.to_owned()))),
    )
}

/// Assert `err` is the canonical reserved-filter-field rejection —
/// attributed to `$filter` with `ValidationReason::Validation`, carrying the
/// *reserved* reason rather than the unpublished one, and naming the
/// offending field.
///
/// **The reason assertion is what makes this helper discriminate.**
/// [`UsageCollectorError::reserved_filter_field`] and
/// [`UsageCollectorError::unpublished_filter_field`] both set
/// `field = "$filter"` and `reason = ValidationReason::Validation`, and both
/// interpolate the offending name into `detail`, so without the
/// `"typed parameter"` check below every caller goes vacuous on the one thing
/// it is named for. That substring appears only in the reserved detail, and
/// `a_filter_naming_the_entry_id_is_refused_as_unpublished` pins from the
/// other side that the unpublished detail does not contain it.
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
            assert!(
                detail.contains("typed parameter"),
                "a reserved field earns the reserved reason, not the unpublished \
                 one: its refusal must say where the field travels instead. Got \
                 {detail}"
            );
        }
        other => panic!("expected InvalidArgument/Validation, got {other:?}"),
    }
}

/// [`assert_reserved_field`]'s opposite number: assert `err` is the
/// *unpublished*-field rejection, naming `field_name` and explicitly **not**
/// carrying the reserved reason.
///
/// Both halves matter, for the reason given on [`assert_reserved_field`].
/// `field_name` is matched in its quoted form, because the unpublished detail
/// also renders the whole admissible set and several of those names contain
/// others as substrings.
fn assert_unpublished_field(err: UsageCollectorError, field_name: &str) {
    match err {
        UsageCollectorError::InvalidArgument {
            field,
            reason,
            detail,
            ..
        } => {
            assert_eq!(
                field, "$filter",
                "unpublished-field violation attributes to $filter"
            );
            assert_eq!(reason, ValidationReason::Validation);
            assert!(
                detail.contains(&format!("'{field_name}'")),
                "detail must name the offending field '{field_name}': {detail}"
            );
            assert!(
                !detail.contains("typed parameter"),
                "'{field_name}' is unpublished, not reserved: telling the caller it \
                 travels as a typed parameter sends them looking for one that does \
                 not exist. Got {detail}"
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

/// Assert `err` is the canonical repeated-grouping-dimension rejection —
/// attributed to `group_by` with `ValidationReason::Validation`,
/// `resource_name` carrying the queried meter — and that its `detail`
/// contains `wire_fragment` (the repeated dimension's wire-rendered form).
///
/// Asserting `reason` is load-bearing beyond this helper's own callers:
/// `two_undeclared_dimensions_report_undeclared_rather_than_repeated`'s only
/// oracle for "declaredness fired, not uniqueness" is that the two
/// constructors carry different reasons (`Validation` vs
/// `UnknownMetadataKey`).
fn assert_repeated_dimension(err: UsageCollectorError, wire_fragment: &str) {
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
                "repeated-dimension violation attributes to group_by"
            );
            assert_eq!(
                reason,
                ValidationReason::Validation,
                "repeated_grouping_dimension must carry a reason distinct from \
                 undeclared_metadata_dimension's UnknownMetadataKey: got detail {detail}"
            );
            assert_eq!(
                resource_name.as_deref(),
                Some(meter_id().as_str()),
                "resource_name must carry the queried meter, matching \
                 undeclared_metadata_dimension's operator-log shape",
            );
            assert!(
                detail.contains(wire_fragment),
                "detail must name the repeated dimension's wire form '{wire_fragment}': {detail}"
            );
        }
        other => panic!("expected InvalidArgument/Validation, got {other:?}"),
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

/// A repeated grouping dimension is refused naming the repeat.
///
/// DESIGN's `AggregationDimension` row says a request names the admissible
/// dimensions "each at most once" and the yaml declares `uniqueItems: true`
/// on `group_by`. Unenforced, a duplicate groups to the same partition and
/// the caller gets a bucket key carrying one value twice — a right number
/// for a malformed request, with nothing saying it was malformed.
#[test]
fn a_repeated_grouping_dimension_is_refused() {
    let dims = vec![
        AggregationDimension::TenantId,
        AggregationDimension::TenantId,
    ];
    let err = require_dimensions_declared(&dims, &declared(&[]), &meter_id())
        .expect_err("a repeated dimension is refused");
    assert_repeated_dimension(err, "tenant_id");
}

/// A repeated **metadata** grouping dimension is refused naming the wire
/// form the caller actually sent — a tagged object (`{"metadata":"region"}`,
/// per `AggregationDimension`'s `#[serde(rename_all = "snake_case")]` with a
/// newtype `Metadata` variant) — not `AggregationDimension`'s Rust-side
/// `Debug` rendering (`Metadata(MetadataKey("region"))`), which
/// `dimension_wire_name` exists to avoid.
///
/// Pins the metadata branch of `dimension_wire_name` specifically: `Debug`
/// renders `TenantId` with no case change, so the fixed-variant test barely
/// discriminates, and a mutation collapsing the metadata branch onto the
/// `Debug` fallback reds nowhere else.
#[test]
fn a_repeated_metadata_dimension_is_refused_naming_the_wire_form() {
    let dims = vec![
        AggregationDimension::Metadata(MetadataKey::new("region").expect("declared")),
        AggregationDimension::Metadata(MetadataKey::new("region").expect("declared")),
    ];
    let err = require_dimensions_declared(&dims, &declared(&["region"]), &meter_id())
        .expect_err("a repeated metadata dimension is refused");
    assert_repeated_dimension(err, r#"{"metadata":"region"}"#);
}

/// ABSENCE PIN — there is no arity ceiling, and there must not become one.
///
/// DESIGN says a request is bounded "never by a fixed ceiling on how many a
/// request may carry", PRD §5.1 says "in any combination and any order", and
/// the yaml's `group_by` description agrees. Pinned rather than merely not
/// built, because an unpinned absence is what let an invented "bounded band"
/// survive.
///
/// Paired with the rejection above because `usage-query.md`'s §6 criterion
/// pairs them: "A grouping list repeating one dimension is rejected, while a
/// list naming every admissible dimension once, in an arbitrary order, is
/// admitted."
#[test]
fn every_admissible_dimension_once_in_an_arbitrary_order_is_admitted() {
    // All five fixed dimensions plus two declared metadata keys, shuffled.
    let dims = vec![
        AggregationDimension::Metadata(MetadataKey::new("region").expect("declared")),
        AggregationDimension::SubjectType,
        AggregationDimension::TenantId,
        AggregationDimension::Metadata(MetadataKey::new("tier").expect("declared")),
        AggregationDimension::ResourceType,
        AggregationDimension::SubjectId,
        AggregationDimension::ResourceId,
    ];
    require_dimensions_declared(&dims, &declared(&["region", "tier"]), &meter_id())
        .expect("seven dimensions, each once, is admissible: arity is bounded by the set");
}

/// A metadata key differing only in case is undeclared, not repeated —
/// because declaredness wins regardless of how the uniqueness pass compares.
///
/// Declaredness is an exact `BTreeSet<String>` match and runs first, so
/// `Region` is refused as undeclared where only `region` is declared,
/// whatever the uniqueness pass would have done. **This does not pin
/// case-sensitivity of the uniqueness comparison** —
/// `two_case_varied_declared_dimensions_are_two_distinct_dimensions` does,
/// with both spellings declared so that pass is reached.
#[test]
fn a_case_varied_metadata_dimension_is_undeclared_rather_than_repeated() {
    let dims = vec![
        AggregationDimension::Metadata(MetadataKey::new("Region").expect("a valid key")),
        AggregationDimension::Metadata(MetadataKey::new("region").expect("a valid key")),
    ];
    let err = require_dimensions_declared(&dims, &declared(&["region"]), &meter_id())
        .expect_err("`Region` is not declared");
    let UsageCollectorError::InvalidArgument {
        ref reason,
        ref detail,
        ..
    } = err
    else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    // The `reason` discriminator, not the `detail` wording, is what proves
    // declaredness fired rather than uniqueness: both details embed the key
    // string and neither contains the literal word "repeat", so a
    // `detail`-only assertion passes under either code path.
    assert_eq!(
        *reason,
        ValidationReason::UnknownMetadataKey,
        "declaredness, not uniqueness, must be what fires; got {detail}"
    );
    assert!(
        detail.contains("Region"),
        "the caller's typo is what they can fix; got {detail}"
    );
    assert!(
        !detail.to_lowercase().contains("repeat"),
        "a typo is not a duplicate; got {detail}"
    );
}

/// REVIEW FOCUS 5 — the uniqueness comparison is exact, so two spellings
/// differing only in case are two dimensions rather than a repeat.
///
/// The only test reaching the uniqueness pass with a case-varied pair: both
/// keys are declared, so declaredness does not short-circuit first. Red under
/// a case-insensitive pass, which would fold the two into one and refuse a
/// legitimate request as a duplicate.
#[test]
fn two_case_varied_declared_dimensions_are_two_distinct_dimensions() {
    let dims = vec![
        AggregationDimension::Metadata(MetadataKey::new("Region").expect("a valid key")),
        AggregationDimension::Metadata(MetadataKey::new("region").expect("a valid key")),
    ];
    require_dimensions_declared(&dims, &declared(&["region", "Region"]), &meter_id())
        .expect("two spellings, both declared, are two distinct keys under exact comparison");
}

/// Declaredness is reported before repetition, for the same reason.
#[test]
fn two_undeclared_dimensions_report_undeclared_rather_than_repeated() {
    let dims = vec![
        AggregationDimension::Metadata(MetadataKey::new("nope").expect("a valid key")),
        AggregationDimension::Metadata(MetadataKey::new("nope").expect("a valid key")),
    ];
    let err = require_dimensions_declared(&dims, &declared(&[]), &meter_id())
        .expect_err("`nope` is not declared");
    let UsageCollectorError::InvalidArgument {
        ref reason,
        ref detail,
        ..
    } = err
    else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    // Both dimensions are the SAME key, so an order bug running uniqueness
    // first would produce a detail containing "nope" too. Only the `reason`
    // discriminates: `undeclared_metadata_dimension` alone sets
    // `UnknownMetadataKey`.
    assert_eq!(
        *reason,
        ValidationReason::UnknownMetadataKey,
        "declaredness, not uniqueness, must be what fires; got {detail}"
    );
    assert!(
        detail.contains("nope") && !detail.to_lowercase().contains("repeat"),
        "an undeclared name is wrong whatever its multiplicity; got {detail}"
    );
}

#[test]
fn gts_type_id_is_reserved_as_a_filter_field() {
    // 3.11: it travels as a typed parameter, so any predicate touching it is
    // rejected rather than silently honored.
    let filter = eq_predicate("gts_type_id");
    let err = reject_unpublished_filter_fields(&filter)
        .expect_err("gts_type_id must be rejected as a filter field");
    assert_reserved_field(err, "gts_type_id");
}

#[test]
fn the_window_bounds_are_not_filterable() {
    for field in ["window_start", "window_end"] {
        let filter = eq_predicate(field);
        let err = reject_unpublished_filter_fields(&filter).expect_err(&format!(
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
    let err = reject_unpublished_filter_fields(&filter)
        .expect_err("a reserved field nested under `or` must still be rejected");
    assert_reserved_field(err, "gts_type_id");
}

#[test]
fn a_reserved_field_nested_under_not_is_rejected() {
    let filter = ast::Expr::Not(Box::new(eq_predicate("window_start")));
    let err = reject_unpublished_filter_fields(&filter)
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
    let err = reject_unpublished_filter_fields(&filter)
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
    let err = reject_unpublished_filter_fields(&filter)
        .expect_err("a reserved field named inside a function argument must be rejected");
    assert_reserved_field(err, "window_start");
}

#[test]
fn a_non_reserved_filter_is_accepted() {
    let filter = ast::Expr::And(
        Box::new(eq_predicate("resource_id")),
        Box::new(eq_predicate("tenant_id")),
    );
    reject_unpublished_filter_fields(&filter).expect("no reserved field named: must be accepted");
}

#[test]
fn reserved_field_match_is_case_insensitive() {
    // A case-varied spelling of a reserved field must not walk past the
    // reservation, which means matching the way the downstream resolver
    // matches: `FilterField::from_name` compares with `eq_ignore_ascii_case`,
    // so `WINDOW_END` folds to the `WindowEnd` variant and would reach a
    // plugin as a real predicate on the column. (The `$orderby` guards next
    // door match exactly, because *their* resolver — the plugin's
    // `record_column` — is an exact `match`.)
    //
    // Load-bearing: a case-varied `gts_type_id` has a second net, since
    // `from_name` reports `UnknownField` for a name off the schema. The
    // covered-period bounds are on the schema, so this reservation is the
    // only thing between `WINDOW_END` in a `$filter` and the plugin.
    for field in ["GTS_TYPE_ID", "Window_Start", "WINDOW_END"] {
        let filter = eq_predicate(field);
        let err = reject_unpublished_filter_fields(&filter).expect_err(&format!(
            "a case-varied spelling of a reserved field ('{field}') must still be rejected"
        ));
        assert_reserved_field(err, field);
    }
}

/// `$filter=id` is refused: the published set is eight fields and `id` is
/// not among them.
///
/// `DESIGN.md`'s `UsageRecordFilterField` row and the yaml's `$filter`
/// parameter enumerate the same set as [`PUBLISHED_FILTER_FIELDS`], and
/// DESIGN adds "Fixed, not resolved per request". The generated filter schema
/// declares `id` beyond it, present there because it is the canonical
/// keyset's final tiebreaker and has to resolve to a column; nothing asks for
/// it to be filterable, and the point lookup is the surface for pinning a
/// record.
#[test]
fn a_filter_naming_the_entry_id_is_refused_as_unpublished() {
    let filter = eq_predicate("id");
    let err = reject_unpublished_filter_fields(&filter)
        .expect_err("id is not on the published filter surface");
    let UsageCollectorError::InvalidArgument {
        ref field,
        ref detail,
        ..
    } = err
    else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "$filter");
    // The **quoted** form, not the bare substring. The unpublished detail
    // renders the whole admissible set, so `detail.contains("id")` holds
    // whether or not the offending name is interpolated at all. Only the
    // offending name is single-quoted — the rendered set comes from `{:?}`
    // over `PUBLISHED_FILTER_FIELDS`, so its members carry double quotes —
    // which is what makes `'id'` identify the interpolation uniquely.
    assert!(
        detail.contains("'id'"),
        "the refusal names the field; got {detail}"
    );
    // The reason must NOT be the reserved-to-a-typed-parameter one: `id`
    // travels as no typed parameter, and telling a caller it does would
    // send them looking for a parameter that does not exist.
    assert!(
        !detail.contains("typed parameter"),
        "id is unpublished, not reserved; got {detail}"
    );
}

/// The reserved three keep their own reason.
#[test]
fn a_filter_naming_a_reserved_bound_still_says_it_travels_as_a_parameter() {
    let filter = eq_predicate("window_end");
    let err = reject_unpublished_filter_fields(&filter).expect_err("window_end is reserved");
    let UsageCollectorError::InvalidArgument { ref detail, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(
        detail.contains("typed parameter"),
        "a reserved field's refusal explains where it travels instead; got {detail}"
    );
}

/// The allowlist does not regress the tree walk.
///
/// Asserts the *unpublished* reason, not merely `is_err()`: `id` is off the
/// published eight and reserved to no typed parameter, so a nested refusal
/// that arrived with the reserved reason would be the wrong refusal, and a
/// bare `is_err()` cannot tell the two apart.
#[test]
fn an_unpublished_field_nested_under_or_not_and_in_is_still_refused() {
    for filter in [
        ast::Expr::Not(Box::new(eq_predicate("id"))),
        eq_predicate("tenant_id").or(eq_predicate("id")),
        ast::Expr::In(
            Box::new(ast::Expr::Identifier("id".to_owned())),
            vec![ast::Expr::Value(ast::Value::String("x".to_owned()))],
        ),
    ] {
        let err = reject_unpublished_filter_fields(&filter).expect_err(&format!(
            "a nested unpublished field is as much a constraint as a top-level one: \
             {filter:?}"
        ));
        assert_unpublished_field(err, "id");
    }
}

/// `In`'s **item list** is walked, not just its left-hand side.
///
/// Every other `In` test here puts the offending identifier on `In`'s left
/// and only `Value`s among the items, so replacing `items.iter().try_for_each(…)`
/// in `reject_unpublished_filter_fields` with `Ok(())` reds none of them. An
/// identifier *is* reachable there: `odata_parse.rs`'s `in` rule parses a
/// `filter_list` whose items are full `filter()` expressions, and
/// `value_expr`'s `property_path` arm yields an `Expr::Identifier` — so
/// `tenant_id in (id)` is grammatical.
#[test]
fn an_unpublished_field_among_an_in_lists_items_is_refused() {
    let filter = ast::Expr::In(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        vec![
            ast::Expr::Value(ast::Value::String("x".to_owned())),
            ast::Expr::Identifier("id".to_owned()),
        ],
    );
    let err = reject_unpublished_filter_fields(&filter)
        .expect_err("an unpublished identifier among `in`'s items must be refused");
    assert_unpublished_field(err, "id");
}

/// The same, with a *reserved* name among the items, so the recursion is
/// pinned on both arms of the guard rather than only the unpublished one.
#[test]
fn a_reserved_field_among_an_in_lists_items_is_refused() {
    let filter = ast::Expr::In(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        vec![ast::Expr::Identifier("window_end".to_owned())],
    );
    let err = reject_unpublished_filter_fields(&filter)
        .expect_err("a reserved identifier among `in`'s items must be refused");
    assert_reserved_field(err, "window_end");
}

/// The negative half of the allowlist's case behaviour: a case-varied
/// spelling of an *inadmissible* field is refused.
///
/// This direction is nearly free — an exact comparison on an allowlist
/// refuses strictly more — so on its own it pins almost nothing about the
/// comparison. `a_case_varied_published_field_is_admitted` is the half that
/// pins it.
#[test]
fn a_case_varied_unpublished_field_is_refused() {
    for spelling in ["ID", "Id", "Window_End", "GTS_TYPE_ID"] {
        let filter = eq_predicate(spelling);
        assert!(
            reject_unpublished_filter_fields(&filter).is_err(),
            "`{spelling}` resolves through `FilterField::from_name` as a real schema \
             field and must not walk past the guard"
        );
    }
}

/// Every published field is admitted — against the eight written out here,
/// not against the constant.
///
/// **The literal list is what makes this a test.** Iterating
/// `PUBLISHED_FILTER_FIELDS` and asserting each member is admitted tests the
/// constant against itself: drop `"origin"` from it and the loop passes while
/// a documented published field starts 400-ing with nothing in this gear red.
/// The plugin's `the_yaml_transcription_matches_the_sdk_published_set`
/// catches that from the contract side; this is the gear-side pin.
///
/// The list is `DESIGN.md`'s `UsageRecordFilterField` row and
/// `usage-collector-v1.yaml`'s `$filter` parameter description, which add
/// "Fixed, not resolved per request".
#[test]
fn the_eight_published_filter_fields_are_all_admitted() {
    const PUBLISHED: [&str; 8] = [
        "tenant_id",
        "resource_id",
        "resource_type",
        "subject_id",
        "subject_type",
        "entry_type",
        "origin",
        "invalidates",
    ];

    for field in PUBLISHED {
        let filter = eq_predicate(field);
        assert!(
            reject_unpublished_filter_fields(&filter).is_ok(),
            "`{field}` is on the published surface and must be admitted"
        );
    }

    // And nothing *but* those: the constant the guard tests against must be
    // exactly this list, so a name quietly added to it cannot widen the
    // surface while every assertion above still passes.
    assert_eq!(
        PUBLISHED_FILTER_FIELDS,
        PUBLISHED.as_slice(),
        "the guard's admissible set must be exactly the published eight"
    );
}

/// The positive direction of the allowlist's case behaviour, and the pin for
/// controller ruling H48.
///
/// `a_case_varied_unpublished_field_is_refused` cannot pin this: an exact
/// comparison on an allowlist refuses strictly more, so every case-varied
/// *unknown* name stays an error either way. This is the test that reds when
/// `eq_ignore_ascii_case` becomes `==`.
///
/// It pins the gear's agreement with
/// `toolkit_odata::filter::FilterField::from_name`, the resolver a `$filter`
/// identifier actually goes through, which compares with
/// `eq_ignore_ascii_case`. An exact comparison here would refuse a predicate
/// the storage layer resolves perfectly well: `TENANT_ID` folds to the
/// `TenantId` variant and `translate_filter` asks for its column as
/// `col(field.name())`, never the caller's string.
#[test]
fn a_case_varied_published_field_is_admitted() {
    for spelling in ["TENANT_ID", "Tenant_Id", "tenant_ID", "ENTRY_TYPE"] {
        let filter = eq_predicate(spelling);
        assert!(
            reject_unpublished_filter_fields(&filter).is_ok(),
            "`{spelling}` resolves through `FilterField::from_name` to a published \
             field and the whole stack handles it; refusing it here would be a \
             gratuitous 400"
        );
    }
}

// reject_off_label_literals — Spec §13 items 11/12 and Review Focus 3: a
// `$filter` comparing a closed-label field (`entry_type`, `origin`) against
// a literal outside its label set is a caller error, not a 500 or a
// silently empty page.

/// Assert `err` is the canonical inadmissible-filter-literal rejection —
/// attributed to `$filter`, naming `value` in its detail — and returns the
/// detail for any test-specific follow-up checks (e.g. that the admissible
/// labels are named too).
///
/// **The `"is not an admissible"` check is what makes this discriminate.**
/// `inadmissible_filter_literal`, `reserved_filter_field` and
/// `unpublished_filter_field` all carry `field: "$filter"` **and**
/// `ValidationReason::Validation` here, so neither tells the constructors
/// apart — unlike `assert_reserved_field`'s surface, where `reason` is the
/// discriminator. A substring of this constructor's own `detail` is the only
/// thing unique to it.
fn assert_inadmissible_literal(err: UsageCollectorError, value: &str) -> String {
    match err {
        UsageCollectorError::InvalidArgument { field, detail, .. } => {
            assert_eq!(
                field, "$filter",
                "inadmissible-literal violation attributes to $filter"
            );
            assert!(
                detail.contains(value),
                "detail must name the offending value '{value}': {detail}"
            );
            assert!(
                detail.contains("is not an admissible"),
                "must be refused by inadmissible_filter_literal specifically, not a neighbour \
                 constructor that also sets field=\"$filter\" and reason=Validation; got {detail}"
            );
            detail
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

/// An off-label `entry_type` literal is a caller error, not a 500.
///
/// `entry_type` is a written enum and the plugin casts the literal, so
/// `entry_type eq 'Record'` raises `PostgreSQL` 22P02, which `classify_db`
/// funnels to a non-transient backend fault — caller input producing a
/// server-class error. The guard is here rather than in the translate layer
/// because `UsageCollectorPluginError` has no invalid-argument variant, so a
/// translate-layer guard would return `Internal` and a 500 anyway.
#[test]
fn an_off_label_entry_type_literal_is_refused() {
    let filter = eq_str("entry_type", "Record");
    let err = reject_off_label_literals(&filter).expect_err("`Record` is not a label");
    let detail = assert_inadmissible_literal(err, "Record");
    assert!(
        detail.contains("record") && detail.contains("invalidation"),
        "and names the admissible labels so the caller can fix it; got {detail}"
    );
}

/// `origin` is the other closed-label column.
///
/// It is `text` with a CHECK constraint rather than an enum, so it raises no
/// 22P02 — an off-label origin answered an empty page. A silently empty
/// answer to a request naming a value outside the closed set is the other
/// half of the same defect.
#[test]
fn an_off_label_origin_literal_is_refused() {
    let filter = eq_str("origin", "imported");
    let err = reject_off_label_literals(&filter).expect_err("`imported` is not a label");
    let detail = assert_inadmissible_literal(err, "imported");
    assert!(
        detail.contains("live") && detail.contains("backfill"),
        "got {detail}"
    );
}

/// REVIEW FOCUS 3 — an `in` list mixing a label with a non-label.
///
/// The guard walks the AST, so an `in` list is a list of literals and each
/// one is checked. A guard that only inspected `Compare` would serve this
/// request from the good half of the list and silently drop the rest.
#[test]
fn an_in_list_mixing_a_label_with_a_non_label_is_refused() {
    let filter = ast::Expr::In(
        Box::new(ast::Expr::Identifier("entry_type".to_owned())),
        vec![
            ast::Expr::Value(ast::Value::String("record".to_owned())),
            ast::Expr::Value(ast::Value::String("Record".to_owned())),
        ],
    );
    let err = reject_off_label_literals(&filter)
        .expect_err("one bad label in an `in` list refuses the whole predicate");
    assert_inadmissible_literal(err, "Record");
}

/// Both labels of both columns are admitted, and so is a filter on a
/// non-closed field.
#[test]
fn every_label_is_admitted_and_open_fields_are_untouched() {
    for (field, label) in [
        ("entry_type", "record"),
        ("entry_type", "invalidation"),
        ("origin", "live"),
        ("origin", "backfill"),
    ] {
        reject_off_label_literals(&eq_str(field, label))
            .unwrap_or_else(|e| panic!("`{field} eq '{label}'` is admissible: {e:?}"));
    }
    // `resource_id` is caller-supplied free text; the guard must not touch it.
    reject_off_label_literals(&eq_str("resource_id", "anything at all"))
        .expect("an open field carries no closed label set");
}

/// Both argument orders: `OData` admits `'Record' eq entry_type` as well as
/// `entry_type eq 'Record'`, and nothing upstream normalizes which side the
/// identifier sits on.
#[test]
fn a_reversed_comparison_order_is_still_checked() {
    let filter = ast::Expr::Compare(
        Box::new(ast::Expr::Value(ast::Value::String("Record".to_owned()))),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Identifier("entry_type".to_owned())),
    );
    let err =
        reject_off_label_literals(&filter).expect_err("'Record' eq entry_type is still off-label");
    assert_inadmissible_literal(err, "Record");
}

/// The guard walks under `and` / `or` / `not`, same as
/// [`reject_unpublished_filter_fields`] — an off-label literal nested under
/// a boolean connective is just as much a constraint on the column as one
/// at the top level.
#[test]
fn an_off_label_literal_nested_under_or_is_refused() {
    let filter = ast::Expr::Or(
        Box::new(eq_predicate("resource_id")),
        Box::new(eq_str("entry_type", "Record")),
    );
    let err = reject_off_label_literals(&filter)
        .expect_err("the `or`'s right arm names an off-label value");
    assert_inadmissible_literal(err, "Record");
}

/// The `Not` arm is a live route to the same 5xx: `toolkit_odata`'s parser
/// emits `Expr::Not` for `not <expr>`, `convert_expr_to_filter_node` maps it
/// to `FilterNode::Not`, and the plugin renders
/// `NOT (entry_type = $1::usage_entry_type)` — so
/// `$filter=not (entry_type eq 'Record')` reaches `PostgreSQL` as a 22P02
/// exactly like the unnested case the moment this arm regresses to
/// `Ok(())`.
#[test]
fn an_off_label_literal_nested_under_not_is_refused() {
    let filter = ast::Expr::Not(Box::new(eq_str("entry_type", "Record")));
    let err =
        reject_off_label_literals(&filter).expect_err("the negated arm names an off-label value");
    assert_inadmissible_literal(err, "Record");
}

/// The guard is operator-blind by design (see [`reject_off_label_literals`]),
/// and for `origin ne` that is a documented permit→deny: `origin ne
/// 'imported'` used to return a semantically correct full page, since no row
/// can hold `'imported'` under the `origin` `CHECK` constraint. It now draws
/// a 400. Deliberate rather than a defect — a predicate naming a value the
/// closed set cannot contain is a confused request on either operator — and
/// pinned so a future refactor that notices `_op` cannot silently undo it.
#[test]
fn an_off_label_origin_not_equals_is_refused_though_the_old_answer_was_a_correct_page() {
    let filter = ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("origin".to_owned())),
        ast::CompareOperator::Ne,
        Box::new(ast::Expr::Value(ast::Value::String("imported".to_owned()))),
    );
    let err = reject_off_label_literals(&filter)
        .expect_err("`ne` is refused exactly like `eq`: the guard never reads the operator");
    assert_inadmissible_literal(err, "imported");
}

// require_metadata_filter_keys_declared — Spec §3.11: `metadata_filter` is
// the dynamic-key side channel `$filter` cannot express a JSON-map key
// predicate through, so it needs its own declared-keys gate, recomputed per
// request exactly like `group_by`'s.

/// Build a `MetadataFilter` for `key` with a single candidate value.
fn metadata_filter(key: &str) -> MetadataFilter {
    MetadataFilter::new(key, ["x".to_owned()]).expect("valid metadata filter")
}

/// A [`MetadataFilter`] on `key` over an explicit value set, for the
/// fingerprint tests further down, where the values are the subject.
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

// require_metadata_filter_within_caps — the published `metadata_filter`
// caps, enforced in the domain so the in-process path is capped too. See
// `query.rs`'s own doc: these caps are what keep a minted cursor inside the
// published `maxLength: 4096`.

/// The caps hold on the in-process path, where no REST test can reach.
///
/// Not a symmetry argument: `query.rs`'s own doc says
/// `MAX_METADATA_FILTERS` / `MAX_METADATA_FILTER_VALUES` are what keep a
/// minted cursor inside the published `maxLength: 4096`, so an uncapped
/// in-process caller can mint an over-length token.
#[test]
fn seventeen_metadata_filters_are_refused_in_process() {
    let filters: Vec<MetadataFilter> = (0..17)
        .map(|i| MetadataFilter::new(format!("k{i}"), ["v"]).expect("a valid filter"))
        .collect();
    let err = require_metadata_filter_within_caps(&filters)
        .expect_err("17 predicates exceeds the cap of 16");
    let UsageCollectorError::InvalidArgument {
        ref field,
        ref detail,
        ..
    } = err
    else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "metadata", "the wire field the REST handler used");
    assert!(
        detail.contains("17") && detail.contains("16"),
        "got {detail}"
    );
}

#[test]
fn thirty_three_values_on_one_key_are_refused_in_process() {
    let values: Vec<String> = (0..33).map(|i| format!("v{i}")).collect();
    let filters = vec![MetadataFilter::new("region", values).expect("a valid filter")];
    let err =
        require_metadata_filter_within_caps(&filters).expect_err("33 values exceeds the cap of 32");
    let UsageCollectorError::InvalidArgument {
        ref field,
        ref detail,
        ..
    } = err
    else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "metadata.region", "the per-key wire field");
    assert!(
        detail.contains("33") && detail.contains("32"),
        "got {detail}"
    );
}

#[test]
fn sixteen_filters_of_thirty_two_values_are_admitted() {
    // The caps are inclusive; the widest admissible request must pass, or
    // the guard is an off-by-one that refuses a legitimate caller.
    let values: Vec<String> = (0..32).map(|i| format!("v{i}")).collect();
    let filters: Vec<MetadataFilter> = (0..16)
        .map(|i| MetadataFilter::new(format!("k{i}"), values.clone()).expect("valid"))
        .collect();
    require_metadata_filter_within_caps(&filters).expect("the widest admissible request");
}

#[test]
fn two_entries_on_one_key_count_as_two_not_one() {
    // The cap is on ENTRIES, not distinct keys: `MetadataFilter`'s own doc
    // says two entries naming one key are AND-ed and "not equivalent to the
    // single filter `k in {a, b}`", so they cost two slots. Entries at the
    // cap, all naming the same key, must be admitted; one more must not.
    let sixteen: Vec<MetadataFilter> = (0..16)
        .map(|_| MetadataFilter::new("region", ["x"]).expect("valid"))
        .collect();
    require_metadata_filter_within_caps(&sixteen)
        .expect("16 entries on one key is 1 distinct key but 16 entries, and 16 is the cap");

    let seventeen: Vec<MetadataFilter> = (0..17)
        .map(|_| MetadataFilter::new("region", ["x"]).expect("valid"))
        .collect();
    let err = require_metadata_filter_within_caps(&seventeen).expect_err(
        "17 entries on one key would be admitted by a distinct-key count (which sees 1 \
         key) but must be refused by an entry count, which is the stricter, correct one",
    );
    let UsageCollectorError::InvalidArgument {
        ref field,
        ref reason,
        ref detail,
        ..
    } = err
    else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "metadata", "the count cap blames the bag");
    assert_eq!(*reason, ValidationReason::Validation);
    assert!(
        detail.contains("17") && detail.contains("16"),
        "got {detail}"
    );
}

// establish_keyset_order — the first-page mode of the keyset floor.
//
// DESIGN §3.1 "Order admissibility" allocates this to the Query Gateway so
// the plugin *always* receives a gap-free, uniform-direction, never-null
// keyset. In the domain rather than the REST handler because an in-process
// caller reaches `Service::list_usage_records` directly with an `ODataQuery`
// of its own construction, which a REST-edge normalization never sees.

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
/// It is therefore useless behind an exact-shape assertion, which implies
/// every property it checks, and is called only from the shape-free
/// table-driven tests. Kept anyway: it is the SPI contract as executable
/// code, which the plugin contract suite starts from.
///
/// The field names are spelled out rather than read from
/// [`CANONICAL_KEYSET_FIELDS`], because an assertion reading the
/// implementation's own constant tests nothing.
/// `the_canonical_keyset_is_the_covered_period_end_and_the_record_id`
/// anchors that constant against the same literals.
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
    // The anchor for every literal `"window_end"` / `"id"` in this file.
    // Spelling them out is what lets those tests catch a repoint, but it also
    // means the constant could gain or lose a field unnoticed. This is the
    // only place the two spellings meet.
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

// The cases below are the shape the guarantee is NOT: a caller order that
// already names `id` keeps it where it is, so the floored order does not end
// in `(window_end, id)`. Every other happy-path test here starts from an
// order naming neither canonical field, which is what left the suite blind
// to this.

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
    //
    // Every name here is *on* the filterable schema and resolves through
    // `UsageRecordFilterField::from_name`; they are refused because the
    // record does not carry them on every entry. That separates this from
    // `an_order_on_created_at_is_rejected` above, which pins the fail-closed
    // refusal of a name the schema does not carry at all.
    for field in ["subject_id", "subject_type", "invalidates"] {
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
fn a_repeated_order_key_is_rejected() {
    // Nothing else dedups an order: `ODataOrderBy::ensure_tiebreaker` only
    // *skips* a field already named rather than deduplicating it out, so a
    // caller order repeating one key would mint a keyset whose JSON-encoded
    // `k` alone exceeds `MAX_KEYSET_BYTES` and a cursor over the published
    // `maxLength`. No document states an order-uniqueness rule; the ground is
    // that cursor budget, which is why this sits with the other keyset-floor
    // rejections rather than being framed as a semantic one.
    let mut query = query_ordered_by(&[
        ("resource_id", SortDir::Asc),
        ("resource_id", SortDir::Asc),
        ("resource_id", SortDir::Asc),
    ]);
    let err = establish_keyset_order(&mut query).expect_err("a repeated order key must be refused");
    assert_orderby_rejection(err);
}

/// The `detail` of a canonical `$orderby` rejection, after
/// [`assert_orderby_rejection`]'s own two checks.
///
/// Exists because those two cannot discriminate: every `keyset_defect`
/// refusal carries `field == "$orderby"` and `reason == Validation`, so the
/// detail is the only thing saying *which* defect fired. Inverting two of
/// `keyset_defect`'s clauses left the whole gear suite green until the test
/// below existed.
fn orderby_rejection_detail(err: UsageCollectorError) -> String {
    match err {
        UsageCollectorError::InvalidArgument {
            field,
            reason,
            detail,
            ..
        } => {
            assert_eq!(field, "$orderby", "the rejection must blame the order");
            assert_eq!(reason, ValidationReason::Validation);
            detail
        }
        other => panic!("expected InvalidArgument on $orderby, got {other:?}"),
    }
}

#[test]
fn the_first_defect_reported_for_a_doubly_defective_order_is_the_actionable_one() {
    // `keyset_defect` returns ONE defect, so the order its rules are checked
    // in decides which refusal a doubly-defective order gets — an order that
    // was neither documented nor pinned, so inverting two clauses left the
    // suite green.
    //
    // Each row is an order tripping two rules at once, paired with the detail
    // fragment the winning refusal carries and the fragment the losing one
    // would. The fragments are the error constructors' own wording, which is
    // what makes this a discrimination rather than a smoke test.
    struct Precedence {
        label: &'static str,
        keys: &'static [(&'static str, SortDir)],
        /// Detail fragment the outranking refusal carries.
        expected: &'static str,
        /// Detail fragment the outranked one would carry instead.
        outranked: &'static str,
    }

    let cases = [
        Precedence {
            // The case the precedence exists for. Deleting the duplicate
            // leaves `$orderby=nope`, still refused — so "you named it
            // twice" sends the caller to fix the multiplicity of a name
            // that was never orderable at any multiplicity.
            label: "inadmissible and repeated",
            keys: &[("nope", SortDir::Asc), ("nope", SortDir::Asc)],
            expected: "is not supported",
            outranked: "named more than once",
        },
        Precedence {
            // One key, named twice, in two directions: mixed-direction and
            // duplicate are both true.
            label: "mixed-direction and repeated",
            keys: &[
                ("resource_id", SortDir::Asc),
                ("resource_id", SortDir::Desc),
            ],
            expected: "must all share one sort direction",
            outranked: "named more than once",
        },
        Precedence {
            // The chain's weaker boundary, pinned so a reordering is loud
            // rather than silent: a direction defect is a property of the
            // whole tuple, a key defect of one key.
            label: "mixed-direction and inadmissible",
            keys: &[("resource_id", SortDir::Asc), ("nope", SortDir::Desc)],
            expected: "must all share one sort direction",
            outranked: "is not supported",
        },
    ];

    for Precedence {
        label,
        keys,
        expected,
        outranked,
    } in cases
    {
        let mut query = query_ordered_by(keys);
        let detail = orderby_rejection_detail(
            establish_keyset_order(&mut query)
                .expect_err("a doubly-defective order must be refused"),
        );
        assert!(
            detail.contains(expected),
            "{label}: the reported defect must be the outranking one \
             ({expected:?}); got {detail:?}",
        );
        assert!(
            !detail.contains(outranked),
            "{label}: the outranked defect ({outranked:?}) must NOT be the \
             one reported - a swap of `keyset_defect`'s clauses would \
             silently re-label this refusal; got {detail:?}",
        );
    }
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
/// neither / one / both canonical fields, in every position, at several
/// caller-key counts, in both directions. Every name here is on
/// `KEYSET_SAFE_RECORD_FIELDS`, which is what makes each shape admissible
/// rather than a rejection in disguise.
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
    // The guarantee and only the guarantee: no exact-shape assertion here,
    // so `assert_keyset_guarantee` is the single thing that can fail. This
    // covers every admissible shape the floor can be handed, catching a
    // mutation that happens to be correct on the written-out ones above.
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

// require_continuation_keyset — the continuation mode of the keyset floor.
//
// An order reconstructed from a token is checked, never extended. The
// token's boundary values (`CursorV1::k`) line up one for one with the keys
// it was minted under, so appending a key would widen the sort tuple past the
// values available to compare against and hand the plugin a misaligned
// continuation — a silently wrong page. Nothing downstream catches it:
// `validate_cursor_against` never checks the token's width against the
// order's.
//
// That the order comes back UNCHANGED is not asserted here — the `&ODataQuery`
// signature enforces it. What a test can still get wrong is the service
// calling the wrong mode, pinned in `read_path_keyset_floor_tests`.

/// Mint a continuation the way a conforming plugin does, then decode it the
/// way the handler does.
///
/// The round trip is the point: `to_signed_tokens` is what a plugin puts in
/// `next_cursor.s`, and `ODataOrderBy::from_signed_tokens` is what
/// `prepare_list_query` calls to rebuild `query.order` from it. Deriving the
/// order FROM the token, rather than setting both from one list, is what
/// keeps these fixtures from being self-consistent by construction.
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

/// Assert `err` is a structural continuation refusal.
///
/// The wire code and the `cursor` attribution are `toolkit_odata`'s (Spec
/// §3.13), asserted where the projection happens in
/// `infra::sdk_error_mapping`. What this pins is that the refusal carries
/// upstream's `InvalidCursor` rather than the query-relevance error, since
/// that choice alone decides the code the caller reads.
fn assert_cursor_rejection(err: UsageCollectorError) {
    match err {
        UsageCollectorError::CursorRejected { source, .. } => assert!(
            matches!(source, toolkit_odata::Error::InvalidCursor),
            "a structural continuation defect must carry upstream's \
             InvalidCursor, got {source:?}",
        ),
        other => panic!("expected a cursor rejection, got {other:?}"),
    }
}

#[test]
fn every_order_the_floor_establishes_is_accepted_as_a_continuation() {
    // The two modes MUST agree, or pagination stops after page one: whatever
    // `establish_keyset_order` hands the plugin comes back as a token, and
    // `require_continuation_keyset` has to accept the order decoded from it.
    // Running the whole admissible table through both in sequence pins that.
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

#[test]
fn a_continuation_naming_a_repeated_key_is_refused_as_a_cursor_defect() {
    // The continuation mirror of `a_repeated_order_key_is_rejected`: a
    // forged, replayed or non-conforming-plugin-minted token can carry a
    // repeated key just as a caller's `$orderby` can, and the continuation
    // mode shares `keyset_defect` with the first-page mode precisely so
    // both close at once.
    let query = continuation_ordered_by(&[
        ("resource_id", SortDir::Asc),
        ("resource_id", SortDir::Asc),
        ("window_end", SortDir::Asc),
        ("id", SortDir::Asc),
    ]);
    let err = require_continuation_keyset(&query)
        .expect_err("a token bound to a repeated-key order must be refused");
    assert_cursor_rejection(err);
}

// read_fingerprint / require_cursor_fingerprint — the query a continuation is
// bound to.
//
// `CursorV1::f` exists so a caller who changes their query between pages is
// refused rather than served a continuation minted over a different row set.
// What decides that row set is the caller's `$filter` plus the typed
// parameters `gts_type_id`, the read range and `metadata_filter`; each is
// pinned separately below, because only `$filter` travels where
// `short_filter_hash` would reach it. Then the shape of the value itself:
// its exact pre-image bytes, its digest, and the length that keeps it out of
// the URL-length failure returning the pre-image caused.

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
    // Stability is NOT asserted by calling the function twice: it is pure, so
    // `f(x) == f(x)` holds under every mutation. The real hazard is stability
    // across *processes* — the value is minted on one request and recomputed
    // on the next, possibly by another replica — which
    // `the_fingerprint_pins_its_exact_bytes` pins, by comparing to a literal.
}

#[test]
fn the_pre_image_of_a_filterless_query_still_carries_the_range() {
    // `short_filter_hash` returns `None` for an absent filter, which is a
    // legitimate complete request (the PDP scope alone narrows it), so a
    // rendering that short-circuited on the `None` would drop the range for
    // exactly the callers who send no `$filter`.
    //
    // Asserted on the pre-image: the fingerprint is a digest, so it contains
    // no field verbatim.
    let plain = ODataQuery::default();
    let range = hour_from(1_700_000_000);
    assert!(
        read_fingerprint_pre_image(&meter_id(), range, &plain, &[])
            .contains(&range.canonical_form()),
        "a filterless query's pre-image must still carry the range",
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

/// Assert `err` is a relevance refusal, not a structural one: the token is
/// structurally fine, it just belongs to another query. The gear spells
/// neither code (Spec §3.13), so the distinction is pinned on the upstream
/// `toolkit_odata` error the wire code is derived from.
fn assert_query_mismatch_rejection(err: UsageCollectorError) {
    match err {
        UsageCollectorError::CursorRejected { source, .. } => assert!(
            matches!(source, toolkit_odata::Error::FilterMismatch),
            "a token minted over another query must carry upstream's \
             FilterMismatch, got {source:?}",
        ),
        other => panic!("expected a cursor rejection, got {other:?}"),
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
    // The over-rejection half, and not theoretical: a REST caller's repeated
    // `metadata.<key>` parameters reach `MetadataFilter::values` in
    // query-string order with duplicates intact, and an in-process caller can
    // hand the entries over in any order. Every spelling below is the same
    // query — OR within a key, AND across keys — so a rendering that read the
    // slice verbatim would refuse a valid page two for a caller who merely
    // re-ordered their own query string.
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
    // Why the rendering is length-prefixed rather than separator-joined. A
    // metadata key is domain-opaque and a value unconstrained, so a field's
    // content can contain whatever separator the rendering picks; a field
    // imitating a boundary makes two different queries render alike, and a
    // cursor minted under one then validates against the other.
    //
    // The first pair is a REAL collision under `field + sep` joining: with
    // `~` as the separator both sides render `…~x~2~1~a~`, because the key
    // `x~2` splits into exactly the two tokens the right side builds from a
    // key and its value count. The `("a", ["bc"])` / `("ab", ["c"])` pair
    // further down does NOT collide, since the interleaved value counts
    // disagree.
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

#[test]
fn the_fingerprint_pre_image_pins_its_exact_bytes() {
    // The golden test, and what makes a fingerprint change diagnosable: it
    // pins the pre-image rather than only the digest, so a rendering change
    // fails here, a hashing change fails the digest test below, and a change
    // to what is bound fails both.
    //
    // The literal is derived from the rule, not captured from a run: field
    // order is meter, filter hash, range, entry count, then per entry the
    // key, its value count and its sorted values, each as `<len>:<bytes>`.
    // The fixture's key and value counts differ, so a dropped count shows.
    //
    // A literal also buys cross-process stability: nothing in this string
    // depends on a process-local seed, a HashMap iteration order, or a
    // `std::hash::Hasher` impl that may differ between builds — and the value
    // is compared on a LATER request, possibly by another replica.
    let range = hour_from(1_700_000_000);
    let metadata = [
        filter_on("region", &["eu", "us"]),
        filter_on("tier", &["gold"]),
    ];

    assert_eq!(
        read_fingerprint_pre_image(&meter_id(), range, &ODataQuery::default(), &metadata),
        // meter (65 bytes, `gts.`-prefixed — that prefix is part of the
        // wire form `MeterTypeId::as_str` returns, and pinning the literal
        // is how this test caught it) | filter hash (absent -> empty)
        // | range | 2 entries | region: 2 values eu, us | tier: 1 value gold
        "65:gts.cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~\
         0:\
         39:1700000000000000000~1700003600000000000\
         1:2\
         6:region1:22:eu2:us\
         4:tier1:14:gold",
    );
}

#[test]
fn the_fingerprint_is_a_sixteen_character_digest_of_that_pre_image() {
    // What reaches the wire. The digest exists because the value is
    // base64url-encoded into the `cursor` URL query parameter, so its length
    // is a wire constraint: the pre-image grows with the caller's own metadata
    // filter (the caps bound how many keys and values may be sent, not how
    // long they are), so a request well inside those caps would otherwise
    // produce a multi-kilobyte cursor and a page-2 URL a proxy refuses with a
    // `414`. A fixed length regardless of input is the property under test.
    let range = hour_from(1_700_000_000);
    let metadata = [
        filter_on("region", &["eu", "us"]),
        filter_on("tier", &["gold"]),
    ];
    let fp = read_fingerprint(&meter_id(), range, &ODataQuery::default(), &metadata);

    // FNV-1a 64 over the pinned pre-image above, rendered as 16 hex
    // digits — the same algorithm and rendering `short_filter_hash`
    // produces for the `$filter` field nested inside it, so this value
    // carries one hashing primitive rather than two that could drift.
    assert_eq!(fp, "2410fb35dc8a65c1");
    assert_eq!(fp.len(), 16, "the wire length must not depend on the input");
}

#[test]
fn the_fingerprint_length_does_not_grow_with_the_metadata_filter() {
    // Deliberately not written against `MAX_METADATA_FILTERS` /
    // `MAX_METADATA_FILTER_VALUES`: the property is size-independent, so
    // pinning it to those caps would add a coupling it does not need.
    // Asserting it far beyond anything the caps admit is stronger and free of
    // that coupling, and it still holds inside them, since
    // `require_metadata_filter_within_caps` runs first on a real request.
    let range = hour_from(1_700_000_000);
    let bare = read_fingerprint(&meter_id(), range, &ODataQuery::default(), &[]);

    let long_values: Vec<String> = (0..64)
        .map(|i| format!("11111111-1111-1111-1111-{i:012}"))
        .collect();
    let wide: Vec<MetadataFilter> = (0..32)
        .map(|i| {
            MetadataFilter::new(format!("key_{i}"), long_values.iter().cloned())
                .expect("valid metadata filter")
        })
        .collect();
    let huge = read_fingerprint(&meter_id(), range, &ODataQuery::default(), &wide);

    assert_eq!(
        huge.len(),
        bare.len(),
        "a wide metadata filter must fingerprint to the same length as no \
         metadata filter at all, or the cursor grows with caller input",
    );
    assert_ne!(
        huge, bare,
        "identical length must not mean the metadata filter stopped counting",
    );
}

#[test]
fn two_identical_metadata_entries_fingerprint_as_one() {
    // `X AND X` is `X`, so this is the same query and must not be refused.
    // REST cannot produce it (`parse_metadata_filters` groups through a
    // `BTreeMap`), but an in-process caller assembles the slice itself. Only
    // WHOLLY identical entries collapse; the same-key-different-values case
    // is the test above.
    let range = hour_from(1_700_000_000);
    let once = [filter_on("region", &["eu"])];
    let twice = [filter_on("region", &["eu"]), filter_on("region", &["eu"])];

    assert_eq!(
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), &once),
        read_fingerprint(&meter_id(), range, &ODataQuery::default(), &twice),
    );
}

// ── `keyset_from_cursor` and `mint_record_cursor` ─────────
//
// `admit_continuation` runs `bind_continuation_order` and
// `require_continuation_keyset` before `keyset_from_cursor` sees the token, so
// `cursor.s` has already produced a sound keyset order. What neither checks is
// `cursor.k`'s own width — a forged, truncated or replayed token can carry a
// `k` disagreeing with the `s` beside it, and `keyset_from_cursor` is the one
// place left that notices. Exercised directly as the pure function it is:
// both failure arms are reachable with a `cursor.s` that passes the floor
// cleanly, so a full-dispatch test would construct one anyway.

fn two_key_order() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "window_end".to_owned(),
            dir: SortDir::Asc,
        },
        OrderKey {
            field: "id".to_owned(),
            dir: SortDir::Asc,
        },
    ])
}

#[test]
fn keyset_from_cursor_refuses_a_token_whose_k_is_narrower_than_the_bound_order() {
    // A sound two-key keyset order, as if `s` had passed
    // `require_continuation_keyset`, but `k` carries one boundary value — a
    // truncated or hand-forged token. `mint_record_cursor` always sets `k`
    // from the plugin's `Keyset`, aligned with its order by construction.
    let order = two_key_order();
    let cursor = CursorV1 {
        k: vec!["2026-01-01T00:00:00Z".to_owned()],
        o: SortDir::Asc,
        s: order.to_signed_tokens(),
        f: Some("fp".to_owned()),
        d: "fwd".to_owned(),
    };

    let err = super::keyset_from_cursor(&cursor, &order)
        .expect_err("one boundary value for a two-key order must be refused");

    match err {
        UsageCollectorError::CursorRejected { source, .. } => assert!(
            matches!(source, toolkit_odata::Error::InvalidCursor),
            "expected upstream's InvalidCursor, got {source:?}",
        ),
        other => panic!("expected a cursor rejection, got {other:?}"),
    }
}

#[test]
fn keyset_from_cursor_refuses_a_token_whose_k_is_wider_than_the_bound_order() {
    // The mirror image: too many boundary values rather than too few. Either
    // arity defect leaves the predicate `keyset_predicate` builds misaligned
    // with the columns it compares against.
    let order = two_key_order();
    let cursor = CursorV1 {
        k: vec![
            "2026-01-01T00:00:00Z".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
            "extra".to_owned(),
        ],
        o: SortDir::Asc,
        s: order.to_signed_tokens(),
        f: Some("fp".to_owned()),
        d: "fwd".to_owned(),
    };

    let err = super::keyset_from_cursor(&cursor, &order)
        .expect_err("three boundary values for a two-key order must be refused");

    assert!(
        matches!(err, UsageCollectorError::CursorRejected { .. }),
        "expected a cursor rejection, got {err:?}",
    );
}

#[test]
fn keyset_from_cursor_refuses_a_forged_oversized_k_the_arity_check_lets_through() {
    // Right arity, wrong size: the arity check above passes it (one value for
    // a one-key order), so `Keyset::new`'s own size bound is the gear's only
    // defence against a forged `k` whose single value exceeds
    // `MAX_KEYSET_BYTES`. A legitimate mint cannot produce this — every mint
    // goes through `Keyset::new`, which refuses the oversized value first.
    let order = ODataOrderBy(vec![OrderKey {
        field: "resource_id".to_owned(),
        dir: SortDir::Asc,
    }]);
    let cursor = CursorV1 {
        k: vec!["x".repeat(usage_collector_sdk::MAX_KEYSET_BYTES + 1)],
        o: SortDir::Asc,
        s: order.to_signed_tokens(),
        f: Some("fp".to_owned()),
        d: "fwd".to_owned(),
    };

    let err = super::keyset_from_cursor(&cursor, &order)
        .expect_err("a boundary value this large must not become a Keyset");

    match err {
        UsageCollectorError::CursorRejected { source, .. } => assert!(
            matches!(source, toolkit_odata::Error::InvalidCursor),
            "expected upstream's InvalidCursor, got {source:?}",
        ),
        other => panic!("expected a cursor rejection, got {other:?}"),
    }
}

#[test]
fn keyset_from_cursor_accepts_a_token_whose_k_matches_the_bound_order() {
    // The admissible case, so the two refusal tests above are proven
    // against a real boundary rather than against a function that always
    // errors.
    let order = two_key_order();
    let cursor = CursorV1 {
        k: vec![
            "2026-01-01T00:00:00Z".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
        ],
        o: SortDir::Asc,
        s: order.to_signed_tokens(),
        f: Some("fp".to_owned()),
        d: "fwd".to_owned(),
    };

    let keyset = super::keyset_from_cursor(&cursor, &order)
        .expect("a token whose k matches the bound order's width is admissible");

    assert_eq!(
        keyset.values(),
        [
            "2026-01-01T00:00:00Z".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
        ],
    );
    assert_eq!(keyset.direction(), SortDir::Asc);
}

#[test]
fn mint_record_cursor_binds_the_dispatched_order_not_the_keysets_own_width() {
    // `mint_record_cursor` takes `order` and `keyset` as two separate
    // parameters rather than deriving `s` from the keyset — `s` is what the
    // *next* dispatch's order is rebuilt from, and that has to be the order
    // this page was actually read under, which [`Keyset`] itself carries no
    // record of.
    let order = two_key_order();
    let keyset = usage_collector_sdk::Keyset::new(
        vec![
            "2026-01-01T00:00:00Z".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
        ],
        SortDir::Asc,
    )
    .expect("a non-empty, in-budget keyset");

    let minted = super::mint_record_cursor(&order, &keyset, "fp-abc");

    assert_eq!(minted.k, keyset.values());
    assert_eq!(minted.o, keyset.direction());
    assert_eq!(minted.s, order.to_signed_tokens());
    assert_eq!(minted.f.as_deref(), Some("fp-abc"));
    assert_eq!(minted.d, "fwd", "the raw path mints forward cursors only");
}

/// One persisted row, so a [`RecordPage`] under test does not also trip the
/// empty-page guard.
fn one_row_page(next: Option<usage_collector_sdk::Keyset>) -> usage_collector_sdk::RecordPage {
    use usage_collector_sdk::{IdempotencyKey, RecordOrigin, ResourceRef, UsageRecord};

    usage_collector_sdk::RecordPage {
        items: vec![
            UsageRecord {
                id: Uuid::from_u128(1),
                gts_type_id: meter_id(),
                tenant_id: Uuid::from_u128(2),
                resource_ref: ResourceRef::new("rsc", "compute.vm").expect("valid resource ref"),
                subject_ref: None,
                metadata: BTreeMap::new(),
                quantity: usage_collector_sdk::UsageQuantity::parse("1").expect("valid quantity"),
                idempotency_key: IdempotencyKey::new("idem-verify-returned-keyset")
                    .expect("valid idempotency key"),
                accepted_at: time::OffsetDateTime::UNIX_EPOCH,
                origin: RecordOrigin::Live,
                invalidation: None,
                window_start: time::OffsetDateTime::UNIX_EPOCH,
                window_end: time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            }
            // The page crosses the SPI in the storage shape. The reference is
            // arbitrary: `verify_returned_keyset` reads the page's length and
            // its keyset, never an entry's meter.
            .into_stored(Uuid::from_u128(0x5E_570_7E5)),
        ],
        next,
    }
}

/// The property `encode_next_cursor_rejects_row_key_order_arity_mismatch`
/// (`plugins/timescaledb-usage-collector-plugin/.../translate_tests.rs`)
/// pinned at the plugin's old mint site, moved here: the plugin no longer
/// mints at all, so the arity check belongs where the gateway mints from
/// whatever a plugin returns.
#[test]
fn verify_returned_keyset_refuses_a_keyset_whose_arity_disagrees_with_the_dispatched_order() {
    let order = two_key_order();
    let page = one_row_page(Some(
        usage_collector_sdk::Keyset::new(["one-value"], SortDir::Asc)
            .expect("a non-empty, in-budget keyset"),
    ));

    let err = super::verify_returned_keyset(&order, &page)
        .expect_err("one boundary value for a two-key order must be refused");

    match err {
        UsageCollectorError::Internal { detail } => assert!(
            detail.contains('1') && detail.contains('2'),
            "the detail must name both widths; got {detail}"
        ),
        other => panic!("expected Internal, got {other:?}"),
    }
}

#[test]
fn verify_returned_keyset_accepts_a_keyset_whose_arity_matches_the_dispatched_order() {
    let order = two_key_order();
    let page = one_row_page(Some(
        usage_collector_sdk::Keyset::new(
            [
                "2026-01-01T00:00:00Z",
                "00000000-0000-0000-0000-000000000001",
            ],
            SortDir::Asc,
        )
        .expect("a non-empty, in-budget keyset"),
    ));

    super::verify_returned_keyset(&order, &page)
        .expect("a matching-arity, matching-direction keyset over a non-empty page is admissible");
}

#[test]
fn verify_returned_keyset_passes_a_page_reporting_no_further_page() {
    // `next: None` is the ordinary last page; nothing to verify.
    let order = two_key_order();
    let page = one_row_page(None);

    super::verify_returned_keyset(&order, &page).expect("no continuation is nothing to verify");
}
