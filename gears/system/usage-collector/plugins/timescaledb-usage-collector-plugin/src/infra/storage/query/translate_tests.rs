// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::TimeZone;

use toolkit_odata::filter::{FieldKind, FilterField, FilterNode, FilterOp};
use toolkit_odata::{CursorV1, ODataOrderBy, OrderKey, SortDir};
use usage_collector_sdk::UsageRecordFilterField;

use super::super::bind::{SqlBind, odata_value_to_bind};
use super::super::keyset::{
    cursor_key_to_bind, ensure_forward_cursor, keyset_predicate, render_order_by, uniform_dir,
};
use super::{ODataValue, SqlCtx, record_column, translate_record_filter};

// ── Helpers ────────────────────────────────────────────────────────────────

/// A field name deliberately absent from [`record_column`]. Shared by the
/// allowlist test and [`UnmappedField`] so the two stay in agreement about what
/// "not a column" means.
const UNMAPPED_FIELD_NAME: &str = "definitely_not_a_column";

/// A [`FilterField`] whose `name()` is [`UNMAPPED_FIELD_NAME`], so a test can
/// reach the translator's fail-closed identifier guard. `FilterField::from_name`
/// resolves only real schema fields, so no parsed query can produce this shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UnmappedField;

impl FilterField for UnmappedField {
    const FIELDS: &'static [Self] = &[Self];

    fn name(&self) -> &'static str {
        UNMAPPED_FIELD_NAME
    }

    fn kind(&self) -> FieldKind {
        FieldKind::String
    }
}

fn rec_field(name: &str) -> UsageRecordFilterField {
    <UsageRecordFilterField as FilterField>::from_name(name)
        .unwrap_or_else(|| panic!("unknown record field `{name}`"))
}

/// Resolve a record field name to its declared [`FieldKind`] — the keyset bind
/// resolver the record store passes to [`keyset_predicate`].
fn rec_kind(name: &str) -> Option<FieldKind> {
    <UsageRecordFilterField as FilterField>::from_name(name).map(|f| f.kind())
}

/// Fail-closed "is this record field a never-null (keyset-safe) column"
/// predicate the record store passes to [`keyset_predicate`].
fn rec_keyset_safe(name: &str) -> bool {
    usage_collector_sdk::is_keyset_safe_record_field(name)
}

fn binary(name: &str, op: FilterOp, value: ODataValue) -> FilterNode<UsageRecordFilterField> {
    FilterNode::Binary {
        field: rec_field(name),
        op,
        value,
    }
}

fn uuid_val() -> ODataValue {
    ODataValue::Uuid(uuid::Uuid::from_u128(0x1234))
}

fn dt_val() -> ODataValue {
    ODataValue::DateTime(chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap())
}

/// The published `$filter` field set (`usage-collector-v1.yaml:440`). Every
/// one of these must resolve to a column, or a valid request is answered with
/// an `Internal`.
///
/// **Hand-copied on purpose.** The other allowlist tests iterate SDK constants,
/// so they are coupled to the code and structurally cannot see the code and the
/// published contract drifting apart. This list is transcribed from the YAML,
/// which is a different source, and so it is the only thing here that catches
/// that drift. Because the published eight are a subset of
/// `<UsageRecordFilterField as FilterField>::FIELDS`, any change to the code
/// alone reds the declared-fields test too; this one fires *alone* exactly when
/// the SDK and the YAML disagree. Refresh it from the YAML, never from the SDK.
///
/// **It guards one direction only.** Nothing here notices the YAML growing a
/// ninth `$filter` field and this transcription not being refreshed: it would
/// stay green while the contract moved out from under it. That is the same
/// staleness the plan warns about, accepted deliberately because the check it
/// buys is unobtainable from any SDK-coupled source — but it is not a safety
/// net in both directions, and a reader should not treat it as one.
const PUBLISHED_FILTER_FIELDS: &[&str] = &[
    "tenant_id",
    "resource_id",
    "resource_type",
    "subject_id",
    "subject_type",
    "entry_type",
    "origin",
    "invalidates",
];

#[test]
fn every_published_filter_field_resolves_to_a_column() {
    for field in PUBLISHED_FILTER_FIELDS {
        assert!(
            record_column(field).is_some(),
            "`$filter={field} eq ...` is a predicate the published contract names \
             and the gear's reject_reserved_filter_fields guard admits, so an \
             allowlist that drops it answers a valid request with a 500"
        );
    }
}

#[test]
fn every_keyset_safe_field_resolves_to_a_column() {
    for field in usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS {
        assert!(
            record_column(field).is_some(),
            "`{field}` is an admissible `$orderby` key, and the canonical \
             (window_end, id) keyset cannot render at all unless it resolves"
        );
    }
}

/// The invariant the other tests approximate. `translate_filter` resolves a
/// conjunct with `col(field.name())`, so *every* declared `FilterField` has to
/// map — a twelfth SDK field would answer a valid `$filter` with a 500 exactly
/// as `origin` did, and neither the published-eight list nor
/// `KEYSET_SAFE_RECORD_FIELDS` would necessarily name it. This also asserts the
/// identity the function's doc claims as a general property rather than as
/// eleven hand-written equalities.
#[test]
fn every_declared_filter_field_maps_to_its_own_column() {
    for field in <UsageRecordFilterField as FilterField>::FIELDS {
        assert_eq!(
            record_column(field.name()),
            Some(field.name()),
            "`{}` is a declared filter field, so a conjunct naming it reaches \
             `col(field.name())` and must resolve to its own column, since \
             the map is documented as the identity",
            field.name()
        );
    }
}

/// The closed half of the boundary. `record_column` is documented as *the*
/// security boundary, and the `$orderby` path (`render_order_by(&query.order,
/// record_column)`) hands it an arbitrary caller-supplied string, unlike the
/// `$filter` path where `FilterField` has already bounded the input. These are
/// real `usage_records` columns, so an arm added for any of them would render
/// valid SQL and widen the boundary silently.
#[test]
fn a_real_column_that_is_not_a_filter_field_does_not_resolve() {
    for column in [
        "reason_code",
        "gts_type_id",
        "value",
        "idempotency_key",
        "acceptance_sequence",
        "metadata",
        "ingested_at",
    ] {
        assert!(
            record_column(column).is_none(),
            "`{column}` is a real usage_records column but not a filter field"
        );
    }
}

#[test]
fn no_retired_model_field_resolves_to_a_column() {
    for field in ["created_at", "corrects_id", "status"] {
        assert!(
            record_column(field).is_none(),
            "`{field}` was removed from the model by slices 3 and 4; an \
             allowlist that still maps it lets a `$filter` naming it past the \
             boundary and fail against the table"
        );
    }
}

// ── Column allowlist ─────────────────────────────────────────────────────────

// The three tests above ask only whether a field resolves. This one names the
// column each field resolves TO: the map is the identity, so a mis-pointed arm
// (`"origin" => Some("tenant_id")`) would satisfy them and silently filter the
// wrong column.
#[test]
fn record_field_columns_are_allowlisted() {
    // The record identity column is `id`; bare `uuid` is not an allowlisted
    // field name.
    assert_eq!(record_column("id"), Some("id"));
    assert_eq!(record_column("uuid"), None);
    assert_eq!(record_column("tenant_id"), Some("tenant_id"));
    assert_eq!(record_column("resource_id"), Some("resource_id"));
    assert_eq!(record_column("resource_type"), Some("resource_type"));
    assert_eq!(record_column("subject_id"), Some("subject_id"));
    assert_eq!(record_column("subject_type"), Some("subject_type"));
    assert_eq!(record_column("entry_type"), Some("entry_type"));
    assert_eq!(record_column("origin"), Some("origin"));
    assert_eq!(record_column("invalidates"), Some("invalidates"));
    assert_eq!(record_column("window_start"), Some("window_start"));
    assert_eq!(record_column("window_end"), Some("window_end"));
    assert_eq!(record_column(UNMAPPED_FIELD_NAME), None);
    // An identifier that would be catastrophic if it were ever interpolated
    // rather than rejected.
    assert_eq!(record_column("id; DROP TABLE usage_records"), None);
}

// ── Value conversion ─────────────────────────────────────────────────────────

#[test]
fn number_converts_to_decimal_bind() {
    let v =
        odata_value_to_bind(&ODataValue::Number(BigDecimal::from_str("42.5").unwrap())).unwrap();
    assert!(matches!(v, SqlBind::Decimal(d) if d.to_string() == "42.5"));
}

#[test]
fn datetime_converts_to_offsetdatetime_bind() {
    let v = odata_value_to_bind(&dt_val()).unwrap();
    assert!(matches!(v, SqlBind::DateTime(_)));
}

#[test]
fn null_and_date_and_time_values_are_rejected() {
    assert!(odata_value_to_bind(&ODataValue::Null).is_err());
    assert!(
        odata_value_to_bind(&ODataValue::Date(
            chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()
        ))
        .is_err()
    );
    assert!(
        odata_value_to_bind(&ODataValue::Time(
            chrono::NaiveTime::from_hms_opt(1, 2, 3).unwrap()
        ))
        .is_err()
    );
}

#[test]
fn bool_and_uuid_values_convert_to_their_binds() {
    assert!(matches!(
        odata_value_to_bind(&ODataValue::Bool(true)).unwrap(),
        SqlBind::Bool(true)
    ));
    let u = uuid::Uuid::from_u128(0x1234);
    assert!(matches!(
        odata_value_to_bind(&ODataValue::Uuid(u)).unwrap(),
        SqlBind::Uuid(got) if got == u
    ));
}

#[test]
fn numeric_out_of_decimal_range_is_rejected() {
    // 40-digit integer: well past rust_decimal::Decimal's 96-bit mantissa, so
    // the `BigDecimal` -> `Decimal` conversion must surface an error rather than
    // silently truncate.
    let huge = BigDecimal::from_str("1000000000000000000000000000000000000000").unwrap();
    assert!(odata_value_to_bind(&ODataValue::Number(huge)).is_err());
}

// ── Filter translation ───────────────────────────────────────────────────────

#[test]
fn binary_eq_renders_single_placeholder_and_one_bind() {
    let node = binary(
        "entry_type",
        FilterOp::Eq,
        ODataValue::String("record".to_owned()),
    );
    let mut ctx = SqlCtx::new(1);
    let sql = translate_record_filter(&node, &mut ctx).unwrap();
    assert_eq!(sql, "entry_type = $1");
    assert_eq!(ctx.binds.len(), 1);
    assert!(matches!(&ctx.binds[0], SqlBind::Str(s) if s == "record"));
}

#[test]
fn composite_and_renders_grouped_predicate_with_two_binds() {
    let node = FilterNode::Composite {
        op: FilterOp::And,
        children: vec![
            binary("tenant_id", FilterOp::Eq, uuid_val()),
            binary("window_end", FilterOp::Ge, dt_val()),
        ],
    };
    let mut ctx = SqlCtx::new(1);
    let sql = translate_record_filter(&node, &mut ctx).unwrap();
    assert_eq!(sql, "(tenant_id = $1 AND window_end >= $2)");
    assert_eq!(ctx.binds.len(), 2);
    assert!(matches!(&ctx.binds[0], SqlBind::Uuid(_)));
    assert!(matches!(&ctx.binds[1], SqlBind::DateTime(_)));
}

#[test]
fn in_list_renders_membership_with_one_placeholder_per_value() {
    let node = FilterNode::InList {
        field: rec_field("tenant_id"),
        values: vec![uuid_val(), uuid_val()],
    };
    let mut ctx = SqlCtx::new(1);
    let sql = translate_record_filter(&node, &mut ctx).unwrap();
    assert_eq!(sql, "tenant_id IN ($1, $2)");
    assert_eq!(ctx.binds.len(), 2);
}

#[test]
fn comparison_operators_render_their_exact_sql() {
    // `=` and `>=` are covered above; assert the remaining four comparison ops
    // emit the exact SQL operator (not just "translation succeeds").
    for (op, sql_op) in [
        (FilterOp::Ne, "<>"),
        (FilterOp::Gt, ">"),
        (FilterOp::Lt, "<"),
        (FilterOp::Le, "<="),
    ] {
        let node = binary("window_end", op, dt_val());
        let mut ctx = SqlCtx::new(1);
        let sql = translate_record_filter(&node, &mut ctx).unwrap();
        assert_eq!(sql, format!("window_end {sql_op} $1"), "op {op:?}");
    }
}

#[test]
fn composite_or_joins_children_with_or_inside_parens() {
    let node = FilterNode::Composite {
        op: FilterOp::Or,
        children: vec![
            binary(
                "entry_type",
                FilterOp::Eq,
                ODataValue::String("record".to_owned()),
            ),
            binary(
                "entry_type",
                FilterOp::Eq,
                ODataValue::String("invalidation".to_owned()),
            ),
        ],
    };
    let mut ctx = SqlCtx::new(1);
    let sql = translate_record_filter(&node, &mut ctx).unwrap();
    assert_eq!(sql, "(entry_type = $1 OR entry_type = $2)");
    assert_eq!(ctx.binds.len(), 2);
}

#[test]
fn empty_in_list_is_rejected() {
    let node = FilterNode::InList {
        field: rec_field("tenant_id"),
        values: vec![],
    };
    let mut ctx = SqlCtx::new(1);
    let err = translate_record_filter(&node, &mut ctx).unwrap_err();
    assert!(err.contains("IN list must not be empty"), "got: {err}");
}

#[test]
fn not_wraps_inner_predicate() {
    let node = FilterNode::Not(Box::new(binary(
        "entry_type",
        FilterOp::Eq,
        ODataValue::String("invalidation".to_owned()),
    )));
    let mut ctx = SqlCtx::new(1);
    let sql = translate_record_filter(&node, &mut ctx).unwrap();
    assert_eq!(sql, "NOT (entry_type = $1)");
}

#[test]
fn placeholder_numbering_honors_start_offset() {
    let node = binary(
        "entry_type",
        FilterOp::Eq,
        ODataValue::String("record".to_owned()),
    );
    let mut ctx = SqlCtx::new(3);
    let sql = translate_record_filter(&node, &mut ctx).unwrap();
    assert_eq!(sql, "entry_type = $3");
}

// The translator must fail closed on a field whose `name()` is not on the
// column allowlist, rather than interpolating it into the SQL.
#[test]
fn record_filter_rejects_field_not_on_column_allowlist() {
    let node = FilterNode::Binary {
        field: UnmappedField,
        op: FilterOp::Eq,
        value: ODataValue::String("active".to_owned()),
    };
    let mut ctx = SqlCtx::new(1);
    let err = translate_record_filter(&node, &mut ctx).unwrap_err();
    assert!(err.contains("not allowlisted"), "got: {err}");
}

#[test]
fn unsupported_operator_is_rejected() {
    let node = binary(
        "resource_id",
        FilterOp::Contains,
        ODataValue::String("vm".to_owned()),
    );
    let mut ctx = SqlCtx::new(1);
    assert!(translate_record_filter(&node, &mut ctx).is_err());
}

// A `Composite` node whose operator is neither `And` nor `Or` (only those two
// are joinable) must be rejected rather than emitting a bogus join. The parser
// never produces this shape, so it is a fail-closed guard on the translator's
// own invariant.
#[test]
fn composite_with_non_and_or_operator_is_rejected() {
    let node = FilterNode::Composite {
        op: FilterOp::Eq,
        children: vec![binary(
            "entry_type",
            FilterOp::Eq,
            ODataValue::String("record".to_owned()),
        )],
    };
    let mut ctx = SqlCtx::new(1);
    let err = translate_record_filter(&node, &mut ctx).unwrap_err();
    assert!(err.contains("invalid composite operator"), "got: {err}");
}

// ── Order-by + keyset ────────────────────────────────────────────────────────

/// The canonical page order this backend renders: `(window_end, id)`, the
/// tiebreaker the published `$orderby` contract appends to every raw-path
/// order.
fn order_window_end_id() -> ODataOrderBy {
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

/// `(window_end ASC, id DESC)` — the shape the SPI guarantees never arrives,
/// and the one every entry point that acts on a direction has to refuse.
fn mixed_direction_order() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "window_end".to_owned(),
            dir: SortDir::Asc,
        },
        OrderKey {
            field: "id".to_owned(),
            dir: SortDir::Desc,
        },
    ])
}

/// The rule itself, at the one place all three entry points now resolve a
/// direction. Two of the three check emptiness first with their own message,
/// so `render_order_by` is the one that relies on the empty arm here — see
/// [`render_order_by_rejects_empty_order`], which pins the composed behaviour.
/// The mixed arm, and an empty order reached by a direct caller, are nameable
/// only here.
#[test]
fn uniform_dir_accepts_one_direction_and_refuses_a_mixed_or_empty_order() {
    assert_eq!(uniform_dir([SortDir::Asc, SortDir::Asc]), Ok(SortDir::Asc));
    assert_eq!(
        uniform_dir([SortDir::Desc, SortDir::Desc]),
        Ok(SortDir::Desc)
    );

    let err = uniform_dir([SortDir::Asc, SortDir::Desc]).unwrap_err();
    assert!(err.contains("mixed-direction"), "got: {err}");
    // The message states a requirement of the call, not the gateway's promise:
    // it prints through `UsageCollectorError::internal` to a caller who cannot
    // see the gateway, and would be asserting that promise exactly when it was
    // broken.
    assert!(
        !err.contains("guarantees"),
        "the reject must not cite a guarantee it is evidence against; got: {err}"
    );

    let err = uniform_dir([]).unwrap_err();
    assert!(err.contains("must not be empty"), "got: {err}");
}

// `render_order_by` runs on the *first* page, before any cursor exists. Mapping
// each key's direction independently renders `window_end ASC, id DESC` and
// serves it, leaving `keyset_predicate` to refuse on the continuation — one
// page too late, and as a 500 over an already-wrong page.
#[test]
fn render_order_by_refuses_a_mixed_direction_order() {
    let err = render_order_by(&mixed_direction_order(), record_column).unwrap_err();
    assert!(err.contains("mixed-direction"), "got: {err}");
}

// `encode_next_cursor` records one direction in the cursor's `o`. Taking it
// from the leading key alone mints a token that *describes* an order its own
// page was not read in, so the continuation looks sound and returns the wrong
// rows. Refuse at mint instead.
#[test]
fn encode_next_cursor_refuses_a_mixed_direction_order() {
    let keys = vec![
        "2026-01-02T03:04:05Z".to_owned(),
        uuid::Uuid::from_u128(1).to_string(),
    ];
    let err = super::super::keyset::encode_next_cursor(
        &mixed_direction_order(),
        &keys,
        Some("filter-hash"),
    )
    .unwrap_err();
    assert!(err.contains("mixed-direction"), "got: {err}");
}

#[test]
fn render_order_by_renders_allowlisted_columns() {
    let sql = render_order_by(&order_window_end_id(), record_column).unwrap();
    assert_eq!(sql, "window_end ASC, id ASC");
}

#[test]
fn render_order_by_rejects_unknown_column() {
    let order = ODataOrderBy(vec![OrderKey {
        field: "not_a_column".to_owned(),
        dir: SortDir::Asc,
    }]);
    assert!(render_order_by(&order, record_column).is_err());
}

/// `render_order_by` has no empty guard of its own: [`uniform_dir`] sees the
/// order first and refuses it there. A duplicate guard here emitted the
/// byte-identical message, so this test passed whether or not it existed —
/// it discriminates the composed path only now that there is one path.
#[test]
fn render_order_by_rejects_empty_order() {
    let err = render_order_by(&ODataOrderBy(vec![]), record_column).unwrap_err();
    assert!(err.contains("order must not be empty"), "got: {err}");
}

#[test]
fn keyset_predicate_rejects_empty_order_pairs() {
    let pairs: &[(&str, bool)] = &[];
    let keys: Vec<String> = vec![];
    let mut ctx = SqlCtx::new(1);
    let err = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap_err();
    assert!(err.contains("keyset order must not be empty"), "got: {err}");
}

#[test]
fn keyset_predicate_rejects_key_order_arity_mismatch() {
    // Two order pairs but a single cursor key: the tuple comparison would be
    // ill-formed, so it must fail closed rather than emit a truncated tuple.
    let pairs: &[(&str, bool)] = &[("window_end", true), ("id", true)];
    let keys = vec!["2026-01-02T03:04:05Z".to_owned()];
    let mut ctx = SqlCtx::new(1);
    let err = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap_err();
    assert!(err.contains("does not match order arity"), "got: {err}");
}

#[test]
fn keyset_predicate_ascending_renders_tuple_comparison_with_two_binds() {
    let pairs: &[(&str, bool)] = &[("window_end", true), ("id", true)];
    let keys = vec![
        "2026-01-02T03:04:05Z".to_owned(),
        uuid::Uuid::from_u128(0x1234).to_string(),
    ];
    let mut ctx = SqlCtx::new(1);
    let sql = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(sql, "(window_end, id) > ($1, $2)");
    assert_eq!(ctx.binds.len(), 2);
    assert!(matches!(&ctx.binds[0], SqlBind::DateTime(_)));
    assert!(matches!(&ctx.binds[1], SqlBind::Uuid(_)));
}

#[test]
fn keyset_predicate_descending_uses_less_than() {
    let pairs: &[(&str, bool)] = &[("window_end", false), ("id", false)];
    let keys = vec![
        "2026-01-02T03:04:05Z".to_owned(),
        uuid::Uuid::from_u128(0x1234).to_string(),
    ];
    let mut ctx = SqlCtx::new(1);
    let sql = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(sql, "(window_end, id) < ($1, $2)");
}

// The SPI guarantees the two canonical names are *present* in `query.order`,
// not that either is last: "a caller ordering by `id` is handed on as `(id,
// window_end)`. A plugin MUST read the order it is given rather than assume a
// position for either key." Sorting the pairs into a canonical shape, or
// hard-coding one, still renders a well-formed tuple and still binds two
// values, so the breakage is silent: the emitted columns stop lining up with
// the cursor keys, and the page resumes from the wrong boundary. Lead with
// `id` so a canonicalising implementation cannot render the same SQL.
#[test]
fn the_predicate_follows_the_order_it_is_given_rather_than_a_canonical_position() {
    let pairs: &[(&str, bool)] = &[("id", true), ("window_end", true)];
    let keys = vec![
        uuid::Uuid::from_u128(0x1234).to_string(),
        "2026-01-02T03:04:05Z".to_owned(),
    ];
    let mut ctx = SqlCtx::new(1);
    let sql = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .expect("an id-led order is admissible; the SPI guarantees presence, not position");
    assert_eq!(
        sql, "(id, window_end) > ($1, $2)",
        "the tuple's columns follow the order handed in, not a canonical one"
    );
    assert_eq!(ctx.binds.len(), 2);
    assert!(
        matches!(&ctx.binds[0], SqlBind::Uuid(_)),
        "the first bind is the first order key's, got {:?}",
        ctx.binds[0]
    );
    assert!(
        matches!(&ctx.binds[1], SqlBind::DateTime(_)),
        "the second bind is the second order key's, got {:?}",
        ctx.binds[1]
    );
}

// A mixed-direction order rendered as a uniform tuple comparison returns the
// wrong rows and reports nothing, so the rule is fail-closed and its violation
// is silent. Both cursor keys must therefore be *parseable* for their field's
// kind: with an unparseable one (`"x"` for the `Uuid`-kinded `id`) the call
// errors inside `cursor_key_to_bind` whatever the directions are, and deleting
// the direction rule outright leaves the assertion green. Assert on the message.
#[test]
fn keyset_predicate_rejects_mixed_directions() {
    let pairs: &[(&str, bool)] = &[("window_end", true), ("id", false)];
    let keys = vec![
        "2026-01-02T03:04:05Z".to_owned(),
        uuid::Uuid::from_u128(1).to_string(),
    ];
    let mut ctx = SqlCtx::new(1);
    let err = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap_err();
    assert!(err.contains("mixed-direction"), "got: {err}");
}

#[test]
fn keyset_predicate_rejects_a_nullable_ordering_column() {
    // Defence-in-depth backstop for finding #3: `subject_type` is an
    // allowlisted, kind-resolvable column, but it is nullable
    // (`subject_ref: Option<_>`). A row-value tuple keyset over it would
    // silently drop NULL-`subject_type` rows, so `keyset_predicate` must fail
    // closed rather than emit `(subject_type) > ($1)` — even though the gateway
    // already rejects such an `$orderby` with a 400, a crafted cursor could
    // still smuggle it in here.
    let pairs: &[(&str, bool)] = &[("subject_type", true)];
    let keys = vec!["vm".to_owned()];
    let mut ctx = SqlCtx::new(1);
    let err = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        err.contains("nullable") && err.contains("subject_type"),
        "error must name the nullable offending field; got: {err}"
    );
    assert!(ctx.binds.is_empty(), "no bind is pushed on the reject path");
}

#[test]
fn keyset_predicate_binds_uuid_column_as_uuid_not_text() {
    // `tenant_id` is a uuid column that is not one of the `*_id`-shaped names
    // the old name-based typing recognized. That heuristic bound it as text,
    // producing a `uuid > text` runtime error. Typing by the field's declared
    // `FieldKind` binds it as Uuid.
    let pairs: &[(&str, bool)] = &[("tenant_id", true)];
    let keys = vec![uuid::Uuid::from_u128(7).to_string()];
    let mut ctx = SqlCtx::new(1);
    let sql = keyset_predicate(
        pairs,
        &keys,
        record_column,
        rec_kind,
        rec_keyset_safe,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(sql, "(tenant_id) > ($1)");
    assert!(
        matches!(ctx.binds[0], SqlBind::Uuid(_)),
        "uuid column binds as Uuid, got {:?}",
        ctx.binds[0]
    );
}

#[test]
fn cursor_key_to_bind_dispatches_on_field_kind() {
    assert!(matches!(
        cursor_key_to_bind(FieldKind::DateTimeUtc, "2026-01-02T03:04:05Z").unwrap(),
        SqlBind::DateTime(_)
    ));
    assert!(matches!(
        cursor_key_to_bind(FieldKind::Uuid, &uuid::Uuid::from_u128(1).to_string()).unwrap(),
        SqlBind::Uuid(_)
    ));
    assert!(matches!(
        cursor_key_to_bind(FieldKind::String, "active").unwrap(),
        SqlBind::Str(_)
    ));
    // Unparseable for its declared kind -> error.
    assert!(cursor_key_to_bind(FieldKind::DateTimeUtc, "not-a-date").is_err());
    assert!(cursor_key_to_bind(FieldKind::Uuid, "not-a-uuid").is_err());
    // A kind with no keyset binding fails closed rather than silently binding
    // as text.
    assert!(cursor_key_to_bind(FieldKind::Decimal, "1.5").is_err());
    assert!(cursor_key_to_bind(FieldKind::I64, "5").is_err());
}

#[test]
fn ensure_forward_cursor_rejects_backward_direction() {
    let mk = |d: &str| CursorV1 {
        k: vec!["x".to_owned()],
        o: SortDir::Asc,
        s: "+window_end".to_owned(),
        f: None,
        d: d.to_owned(),
    };
    assert!(
        ensure_forward_cursor(&mk("fwd")).is_ok(),
        "a forward cursor is accepted"
    );
    // The keyset comparison operator is derived from the sort direction, not
    // from `d`, so a backward cursor would silently be walked forward. It must
    // be rejected fail-closed until backward paging is actually implemented.
    assert!(
        ensure_forward_cursor(&mk("bwd")).is_err(),
        "a backward cursor is rejected"
    );
}

// ── Cursor round-trip ────────────────────────────────────────────────────────

#[test]
fn encode_then_decode_cursor_round_trips_keys_and_order() {
    let order = order_window_end_id();
    let keys = vec![
        "2026-01-02T03:04:05Z".to_owned(),
        uuid::Uuid::from_u128(0x1234).to_string(),
    ];
    let token = super::super::keyset::encode_next_cursor(&order, &keys, Some("hash")).unwrap();
    let decoded = super::super::keyset::decode_cursor(&token).unwrap();
    assert_eq!(decoded.k, keys);
    assert_eq!(decoded.s, "+window_end,+id");
    assert_eq!(decoded.d, "fwd");
    assert_eq!(decoded.f.as_deref(), Some("hash"));
}

#[test]
fn encode_next_cursor_rejects_row_key_order_arity_mismatch() {
    // A two-key order but a single last-row key: the cursor would encode fewer
    // keys than the order it claims to follow, so it must fail closed.
    let order = order_window_end_id();
    let keys = vec!["2026-01-02T03:04:05Z".to_owned()];
    let err = super::super::keyset::encode_next_cursor(&order, &keys, None).unwrap_err();
    assert!(err.contains("does not match order arity"), "got: {err}");
}

#[test]
fn decode_cursor_rejects_a_malformed_client_token() {
    // Cursor tokens are client-supplied and untrusted: a garbage token must
    // surface an error, never a partially-decoded `CursorV1` or a panic.
    assert!(super::super::keyset::decode_cursor("not-a-valid-cursor-token").is_err());
    assert!(super::super::keyset::decode_cursor("").is_err());
    // Valid base64url, but the decoded bytes are not a `CursorV1` JSON payload.
    assert!(super::super::keyset::decode_cursor("bm90LWpzb24").is_err());
}
