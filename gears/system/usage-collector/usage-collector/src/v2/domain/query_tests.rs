//! Unit tests for [`compose_query_with_scope`], for
//! [`reject_unpublished_filter_fields`] /
//! [`require_metadata_filter_keys_declared`] — the Spec §3.11 gate on the
//! admissible `$filter` / `metadata_filter` surface.
//!
//! `require_dimensions_declared` (the same gate's `group_by` half) is
//! unit-tested in `aggregate_query_tests.rs`; `establish_keyset_order` /
//! `require_continuation_keyset` (the raw path's keyset floor in its two
//! modes) and `read_fingerprint` / `require_cursor_fingerprint` (the query a
//! continuation is bound to) are unit-tested in `list_query_tests.rs` — split
//! out of this module so each test sits beside the production module it
//! exercises.
//!
//! There is no bounded-window guard to test: the mandatory read range is a
//! typed [`usage_collector_sdk::v2::TimeRange`] parameter on both read paths
//! rather than a `$filter` conjunct, so nothing about it can be absent,
//! one-sided, or hidden under an `or`.

use std::collections::BTreeSet;

use toolkit_odata::{ODataQuery, ast};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use usage_collector_sdk::v2::{MetadataFilter, MeterTypeId, UsageCollectorError, ValidationReason};
use uuid::Uuid;

use super::{
    PUBLISHED_FILTER_FIELDS, compose_query_with_scope, reject_off_label_literals,
    reject_unpublished_filter_fields, require_metadata_filter_keys_declared,
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
    let (composed, _scope) = compose_query_with_scope(&user, &scope).expect("compose ok");

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

// reject_unpublished_filter_fields — Spec §3.11: the admissible `$filter`
// surface is the fixed `PUBLISHED_FILTER_FIELDS` set; a predicate naming any
// other field is rejected, whether reserved (travels as a typed parameter)
// or simply not on the schema.

fn declared(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_owned()).collect()
}

/// A fixed queried-meter id for tests that don't care which meter, only
/// that `require_metadata_filter_keys_declared` threads it onto the error.
fn meter_id() -> MeterTypeId {
    const GTS_ID: &str =
        toolkit_gts::gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");
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

/// `$filter=id` is refused: `id` is not among the published filter fields.
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
/// `usage-collector-v2.yaml`'s `$filter` parameter description, which add
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

/// The positive direction of the allowlist's case behaviour.
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

// reject_off_label_literals — Spec §13 items 11/12: a
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

/// An `in` list mixing a label with a non-label.
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
/// 'imported'` draws a 400, since no row can hold `'imported'` under the
/// `origin` `CHECK` constraint. Deliberate rather than a defect — a predicate naming a value the
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

#[cfg(test)]
mod private_helpers_tests {
    //! Unit coverage for the private service-module helper
    //! `compose_query_with_scope`.

    use toolkit_odata::ODataQuery;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
    use usage_collector_sdk::v2::UsageCollectorError;
    use uuid::Uuid;

    use crate::v2::domain::query::compose_query_with_scope;

    #[test]
    fn allow_all_scope_is_denied_fail_closed() {
        // An `allow_all` scope on the LIST/aggregate path is a degenerate
        // empty-predicate permit, not a happy-path grant. Composition MUST fail
        // closed rather than pass the user filter through unscoped (which would
        // return every tenant's records).
        let user_filter = Expr::Compare(
            Box::new(Expr::Identifier("resource_type".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("compute.vm".into()))),
        );
        let mut user_query = ODataQuery::new();
        user_query.filter = Some(Box::new(user_filter));
        user_query.limit = Some(50);

        let err = compose_query_with_scope(&user_query, &AccessScope::allow_all())
            .expect_err("allow_all -> PermissionDenied");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "allow_all must surface as PermissionDenied, got {err:?}",
        );
    }

    #[test]
    fn empty_user_filter_yields_scope_filter_alone() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(0xAA),
        )]));
        let (composed, _scope) =
            compose_query_with_scope(&ODataQuery::new(), &scope).expect("happy path");
        let f = composed.filter().expect("scope-only filter");
        assert!(matches!(f, Expr::Compare(..)));
    }

    #[test]
    fn user_filter_and_scope_are_and_merged() {
        let user_filter = Expr::Compare(
            Box::new(Expr::Identifier("resource_type".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("compute.vm".into()))),
        );
        let mut user_query = ODataQuery::new();
        user_query.filter = Some(Box::new(user_filter));

        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(0xBB),
        )]));
        let (composed, _scope) = compose_query_with_scope(&user_query, &scope).expect("happy path");
        let f = composed.filter().expect("merged filter");
        assert!(matches!(f, Expr::And(..)));
    }

    #[test]
    fn deny_all_scope_short_circuits_to_authorization_error() {
        let err = compose_query_with_scope(&ODataQuery::new(), &AccessScope::deny_all())
            .expect_err("deny_all -> PermissionDenied");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "deny_all must surface as PermissionDenied, got {err:?}",
        );
    }
}
