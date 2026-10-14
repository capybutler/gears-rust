//! Unit tests for the raw read path orchestration built in
//! [`crate::v2::domain::list_query`] (`Service::list_usage_records`).
//!
//! The query-surface gate itself (`compose_query_with_scope`,
//! `reject_unpublished_filter_fields` / `require_metadata_filter_keys_declared`,
//! `reject_off_label_literals`) is unit-tested in `query_tests.rs`, and
//! `require_dimensions_declared` in `aggregate_query_tests.rs`; this module's
//! own keyset/cursor pure functions (`establish_keyset_order` /
//! `require_continuation_keyset`, `read_fingerprint` /
//! `require_cursor_fingerprint`, `keyset_from_cursor` / `mint_record_cursor` /
//! `verify_returned_keyset`) are unit-tested directly below. The `mod`s past
//! that point prove the whole gate is wired into the service at the right
//! position (before dispatch) with the right arguments (the resolved
//! declaration's `declared_keys`), and cover what only a `Service` fixture can
//! show: the mandatory typed time range reaching the SPI unchanged, the
//! keyset floor in both its modes, the cursor's binding to the query it was
//! minted under, the gateway-owned cursor lifecycle (the plugin sees a
//! structured `Keyset` and never a wire token), and the verification run on
//! whatever keyset the plugin hands back before a token is minted from it.
//!
//! Not here: the raw-list half of withdrawal-exclusion coverage (that the
//! raw path returns every entry as persisted, withdrawn or not — the ledger
//! view a locally-folding consumer reads). That assertion lives on
//! `aggregate_query_tests.rs::withdrawal_exclusion_tests::
//! a_withdrawn_pair_folds_to_nothing_while_both_stay_readable`, whose
//! dominant subject is the aggregate fold it is named for.

// The keyset/cursor pure functions built in `list_query.rs`:
// `establish_keyset_order` / `require_continuation_keyset` (the raw path's
// keyset floor in its two modes), `read_fingerprint` /
// `require_cursor_fingerprint` (the query a continuation is bound to), and
// `keyset_from_cursor` / `mint_record_cursor` / `verify_returned_keyset` (the
// gateway-owned cursor's own arity checks). Split out of `query_tests.rs`,
// alongside everything else in this file.

use std::collections::BTreeMap;

use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
use usage_collector_sdk::v2::{
    MetadataFilter, MeterTypeId, UsageCollectorError, ValidationReason, is_keyset_safe_record_field,
};
use uuid::Uuid;

use crate::v2::domain::list_query::{
    CANONICAL_KEYSET_FIELDS, establish_keyset_order, keyset_from_cursor, mint_record_cursor,
    read_fingerprint, read_fingerprint_pre_image, require_continuation_keyset,
    require_cursor_fingerprint, verify_returned_keyset,
};

/// Build an [`ODataQuery`] whose `$filter` is the parsed `filter` string.
///
/// Duplicated from `query_tests.rs`: a `#[path]` test child cannot see a
/// sibling module's private helpers, and this builder is two lines.
fn query_with_filter(filter: &str) -> ODataQuery {
    let expr = toolkit_odata::parse_filter_string(filter)
        .expect("test filter parses")
        .into_expr();
    ODataQuery::from(Some(expr))
}

/// A fixed queried-meter id, used wherever a test needs *a* meter without
/// caring which one.
///
/// Duplicated from `query_tests.rs` / `aggregate_query_tests.rs` for the same
/// reason as [`query_with_filter`].
fn meter_id() -> MeterTypeId {
    const GTS_ID: &str =
        toolkit_gts::gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");
    MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
}

/// A [`MetadataFilter`] on `key` over an explicit value set, for the
/// fingerprint tests further down, where the values are the subject.
fn filter_on(key: &str, values: &[&str]) -> MetadataFilter {
    MetadataFilter::new(key, values.iter().copied()).expect("valid metadata filter")
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
        other => unreachable!("expected InvalidArgument on $orderby, got {other:?}"),
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
        other => unreachable!("expected InvalidArgument on $orderby, got {other:?}"),
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
            .unwrap_or_else(|e| unreachable!("{keys:?} is an admissible order, but: {e:?}"));
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
        other => unreachable!("expected a cursor rejection, got {other:?}"),
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
            .unwrap_or_else(|e| unreachable!("{keys:?} is an admissible order, but: {e:?}"));
        assert_keyset_guarantee(&first_page);

        let follow_up = continuation_bound_to(&first_page.order);
        require_continuation_keyset(&follow_up).unwrap_or_else(|e| {
            unreachable!(
                "a token minted from the floored form of {keys:?} must be accepted, but: {e:?}"
            )
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
fn fingerprint(query: &ODataQuery, range: usage_collector_sdk::v2::TimeRange) -> String {
    read_fingerprint(&meter_id(), range, query, &[])
}

/// A range starting at `secs` past the epoch and running one hour.
fn hour_from(secs: i64) -> usage_collector_sdk::v2::TimeRange {
    let from = time::OffsetDateTime::from_unix_timestamp(secs).expect("in-range timestamp");
    usage_collector_sdk::v2::TimeRange::new(from, from + time::Duration::hours(1)).expect("range")
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
    let coarse = usage_collector_sdk::v2::TimeRange::new(from, from + time::Duration::hours(1))
        .expect("range");
    let nudged = usage_collector_sdk::v2::TimeRange::new(
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
        other => unreachable!("expected a cursor rejection, got {other:?}"),
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
        "cf.core.uc.usage_record.v2~cf.mini_chat._.other_meter.v1~"
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
        "65:gts.cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~\
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
    assert_eq!(fp, "a717162fe5d1bc84");
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

    let err = keyset_from_cursor(&cursor, &order)
        .expect_err("one boundary value for a two-key order must be refused");

    match err {
        UsageCollectorError::CursorRejected { source, .. } => assert!(
            matches!(source, toolkit_odata::Error::InvalidCursor),
            "expected upstream's InvalidCursor, got {source:?}",
        ),
        other => unreachable!("expected a cursor rejection, got {other:?}"),
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

    let err = keyset_from_cursor(&cursor, &order)
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
        k: vec!["x".repeat(usage_collector_sdk::v2::MAX_KEYSET_BYTES + 1)],
        o: SortDir::Asc,
        s: order.to_signed_tokens(),
        f: Some("fp".to_owned()),
        d: "fwd".to_owned(),
    };

    let err = keyset_from_cursor(&cursor, &order)
        .expect_err("a boundary value this large must not become a Keyset");

    match err {
        UsageCollectorError::CursorRejected { source, .. } => assert!(
            matches!(source, toolkit_odata::Error::InvalidCursor),
            "expected upstream's InvalidCursor, got {source:?}",
        ),
        other => unreachable!("expected a cursor rejection, got {other:?}"),
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

    let keyset = keyset_from_cursor(&cursor, &order)
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
    let keyset = usage_collector_sdk::v2::Keyset::new(
        vec![
            "2026-01-01T00:00:00Z".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
        ],
        SortDir::Asc,
    )
    .expect("a non-empty, in-budget keyset");

    let minted = mint_record_cursor(&order, &keyset, "fp-abc");

    assert_eq!(minted.k, keyset.values());
    assert_eq!(minted.o, keyset.direction());
    assert_eq!(minted.s, order.to_signed_tokens());
    assert_eq!(minted.f.as_deref(), Some("fp-abc"));
    assert_eq!(minted.d, "fwd", "the raw path mints forward cursors only");
}

/// One persisted row, so a [`RecordPage`] under test does not also trip the
/// empty-page guard.
fn one_row_page(
    next: Option<usage_collector_sdk::v2::Keyset>,
) -> usage_collector_sdk::v2::RecordPage {
    use usage_collector_sdk::v2::{IdempotencyKey, RecordOrigin, ResourceRef, UsageRecord};

    usage_collector_sdk::v2::RecordPage {
        items: vec![
            UsageRecord {
                id: Uuid::from_u128(1),
                gts_type_id: meter_id(),
                tenant_id: Uuid::from_u128(2),
                resource_ref: ResourceRef::new("rsc", "compute.vm").expect("valid resource ref"),
                subject_ref: None,
                metadata: BTreeMap::new(),
                quantity: usage_collector_sdk::v2::UsageQuantity::parse("1")
                    .expect("valid quantity"),
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
        usage_collector_sdk::v2::Keyset::new(["one-value"], SortDir::Asc)
            .expect("a non-empty, in-budget keyset"),
    ));

    let err = verify_returned_keyset(&order, &page)
        .expect_err("one boundary value for a two-key order must be refused");

    match err {
        UsageCollectorError::Internal { detail } => assert!(
            detail.contains('1') && detail.contains('2'),
            "the detail must name both widths; got {detail}"
        ),
        other => unreachable!("expected Internal, got {other:?}"),
    }
}

#[test]
fn verify_returned_keyset_accepts_a_keyset_whose_arity_matches_the_dispatched_order() {
    let order = two_key_order();
    let page = one_row_page(Some(
        usage_collector_sdk::v2::Keyset::new(
            [
                "2026-01-01T00:00:00Z",
                "00000000-0000-0000-0000-000000000001",
            ],
            SortDir::Asc,
        )
        .expect("a non-empty, in-budget keyset"),
    ));

    verify_returned_keyset(&order, &page)
        .expect("a matching-arity, matching-direction keyset over a non-empty page is admissible");
}

#[test]
fn verify_returned_keyset_passes_a_page_reporting_no_further_page() {
    // `next: None` is the ordinary last page; nothing to verify.
    let order = two_key_order();
    let page = one_row_page(None);

    verify_returned_keyset(&order, &page).expect("no continuation is nothing to verify");
}

// The mandatory read range is a typed `TimeRange` parameter on both read
// paths, never a `$filter` conjunct. Two things need pinning, neither visible
// in a status code: that the caller's range is the one the SPI is handed, and
// that a caller supplying no `$filter` is a complete request. A range dropped
// between gateway and plugin is an unbounded scan that still answers `Ok`, so
// every assertion here is on what the plugin received.
mod read_path_time_range_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::v2::{
        AggregationResult, MeterTypeId, RecordPage, TimeRange, UsageCollectorPluginV2,
    };

    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A `Service` plus its [`RecordingPlugin`] spy, over a declaration with
    /// no metadata keys and the fixed-tenant PDP fake both read paths require.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    #[tokio::test]
    async fn list_forwards_the_typed_time_range_to_the_plugin() {
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let range = test_time_range();
        svc.list_usage_records(&ctx(), meter_id(), range, &ODataQuery::default(), &[])
            .await
            .expect("list succeeds");

        assert_eq!(
            spy.last_list_time_range(),
            Some(range),
            "the caller's range MUST reach the SPI verbatim: a substituted or \
             dropped range is an unbounded scan that still answers Ok",
        );
    }

    #[tokio::test]
    async fn aggregate_forwards_the_typed_time_range_to_the_plugin() {
        let (svc, spy) = svc_and_spy();
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let range = test_time_range();
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            range,
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("aggregate succeeds");

        assert_eq!(
            spy.last_aggregate_time_range(),
            Some(range),
            "the aggregate path has no page-size ceiling, so the range is its \
             only scan bound and MUST reach the SPI verbatim",
        );
    }

    #[tokio::test]
    async fn the_range_the_plugin_receives_tracks_the_one_the_caller_passed() {
        // The forwarding tests above would pass against a service that
        // hard-coded a single range; two dispatches with two different ranges
        // through the same service rule that out.
        let (svc, spy) = svc_and_spy();

        let first = test_time_range();
        let second = TimeRange::new(
            first.upper_exclusive(),
            first.upper_exclusive() + time::Duration::hours(1),
        )
        .expect("the adjacent hour is a valid range");
        assert_ne!(first, second, "precondition: the two ranges differ");

        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        svc.list_usage_records(&ctx(), meter_id(), first, &ODataQuery::default(), &[])
            .await
            .expect("first list succeeds");
        assert_eq!(spy.last_list_time_range(), Some(first));

        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        svc.list_usage_records(&ctx(), meter_id(), second, &ODataQuery::default(), &[])
            .await
            .expect("second list succeeds");
        assert_eq!(
            spy.last_list_time_range(),
            Some(second),
            "the second dispatch MUST carry the second range, not a range \
             fixed at construction or cached from the first call",
        );
    }

    /// `PageInfo.limit` is clamped, not merely defaulted, because an
    /// in-process caller's `query.limit` never passes through REST's
    /// `prepare_list_query` `$top` cap the way a REST request's does.
    ///
    /// `RecordPage` carries no limit for the plugin to report, so the gear
    /// computes `PageInfo.limit` from `composed.limit`. Unclamped,
    /// `limit: Some(5000)` would report `5000` on a page a conforming plugin
    /// still caps at `MAX_PAGE_SIZE`, and `limit: Some(0)` would report `0` on
    /// a page that served one row — and `local_client.rs` forwards the page
    /// verbatim, so either number reaches a calling gear as a false claim.
    #[tokio::test]
    async fn an_in_process_limit_outside_the_page_size_bounds_is_clamped_on_the_way_out() {
        use crate::v2::domain::service::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let mut over = ODataQuery::new();
        over.limit = Some(5000);
        let page = svc
            .list_usage_records(&ctx(), meter_id(), test_time_range(), &over, &[])
            .await
            .expect("list succeeds");
        assert_eq!(
            page.page_info.limit, MAX_PAGE_SIZE,
            "a limit above the page-size ceiling must be clamped down to it, \
             not reported as the caller's own oversized request",
        );

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let mut zero = ODataQuery::new();
        zero.limit = Some(0);
        let page = svc
            .list_usage_records(&ctx(), meter_id(), test_time_range(), &zero, &[])
            .await
            .expect("list succeeds");
        assert_eq!(
            page.page_info.limit, 1,
            "a limit of zero must be clamped up to the floor of 1, not \
             reported as zero",
        );

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let page = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect("list succeeds");
        assert_eq!(
            page.page_info.limit, DEFAULT_PAGE_SIZE,
            "an absent limit reports the gear's own default, mirroring the \
             production plugin's unset-limit default",
        );
    }

    /// A `$filter` that constrains something other than time — the shape a
    /// caller wanting both a predicate and a range sends. The forwarding tests
    /// above pass `ODataQuery::default()`, so the pair covers both "no
    /// `$filter` at all" and "a `$filter` naming no time predicate".
    fn filter_without_a_time_predicate() -> ODataQuery {
        ODataQuery::from(Some(
            toolkit_odata::parse_filter_string("resource_id eq 'r1'")
                .expect("filter parses")
                .into_expr(),
        ))
    }

    #[tokio::test]
    async fn list_needs_no_time_window_inside_the_filter() {
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let range = test_time_range();
        svc.list_usage_records(
            &ctx(),
            meter_id(),
            range,
            &filter_without_a_time_predicate(),
            &[],
        )
        .await
        .expect("a $filter naming no time predicate is a complete list request");

        assert_eq!(
            spy.last_list_time_range(),
            Some(range),
            "a non-empty $filter must not disturb the typed range on its way \
             to the SPI",
        );
    }

    #[tokio::test]
    async fn aggregate_needs_no_time_window_inside_the_filter() {
        let (svc, spy) = svc_and_spy();
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let range = test_time_range();
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            range,
            &filter_without_a_time_predicate(),
            &[],
            &[],
        )
        .await
        .expect("a $filter naming no time predicate is a complete aggregate request");

        assert_eq!(
            spy.last_aggregate_time_range(),
            Some(range),
            "a non-empty $filter must not disturb the typed range on its way \
             to the SPI",
        );
    }
}

// The keyset floor and gateway-owned cursor are stated in
// [`crate::v2::domain::query`]. These tests cover the surface no REST test can
// reach: a caller holding a `Service` and handing it an `ODataQuery` of their
// own construction. An unfloored order is not visible in a status code — it is
// a keyset the plugin cannot continue, or one that silently drops rows at a
// page boundary — so every assertion is on the order the plugin received.
mod read_path_keyset_floor_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::v2::{
        MeterTypeId, RecordPage, UsageCollectorError, UsageCollectorPluginV2,
    };

    use crate::v2::domain::Service;
    use crate::v2::domain::list_query::read_fingerprint;
    use crate::v2::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A `Service` over the [`RecordingPlugin`] spy, a declaration with no
    /// metadata keys, and the fixed-tenant PDP fake both read paths require.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
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

    /// The order the spy last received, as owned `(field, direction)` pairs.
    fn received_order(spy: &RecordingPlugin) -> Vec<(String, SortDir)> {
        spy.last_list_order()
            .expect("the plugin MUST have been dispatched")
    }

    fn expected(keys: &[(&str, SortDir)]) -> Vec<(String, SortDir)> {
        keys.iter()
            .map(|(field, dir)| ((*field).to_owned(), *dir))
            .collect()
    }

    #[tokio::test]
    async fn an_in_process_caller_with_no_order_still_reaches_the_plugin_with_the_canonical_keyset()
    {
        // The reason the floor lives in the domain rather than in
        // `prepare_list_query`: an in-process caller hands the service an
        // `ODataQuery::default()`, whose order is empty, and the SPI documents
        // `query.order` as populated.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await
        .expect("an empty order is a complete in-process list request");

        assert_eq!(
            received_order(&spy),
            expected(&[("window_end", SortDir::Asc), ("id", SortDir::Asc)]),
            "the plugin MUST receive the canonical keyset, not the caller's \
             empty order",
        );
    }

    #[tokio::test]
    async fn an_in_process_caller_order_keeps_its_keys_and_gains_the_suffix() {
        // The floor is a floor, not a substitution: a sound caller order
        // survives and only gains the canonical fields it does not name, in
        // its own direction, so the row-value comparison stays uniform.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &query_ordered_by(&[("resource_id", SortDir::Desc)]),
            &[],
        )
        .await
        .expect("a uniform order on a mandatory attribute is admissible");

        assert_eq!(
            received_order(&spy),
            expected(&[
                ("resource_id", SortDir::Desc),
                ("window_end", SortDir::Desc),
                ("id", SortDir::Desc),
            ]),
        );
    }

    #[tokio::test]
    async fn an_in_process_order_the_floor_cannot_repair_never_reaches_the_plugin() {
        // Mixed directions and a nullable leading key are unsound keysets
        // appending a suffix cannot fix, so the service refuses before
        // dispatch. `created_at` is in the same bucket: it is not a record
        // attribute, and the classification is a fail-closed allowlist.
        for order in [
            vec![("resource_id", SortDir::Asc), ("tenant_id", SortDir::Desc)],
            vec![("subject_id", SortDir::Asc)],
            vec![("created_at", SortDir::Asc)],
        ] {
            let (svc, spy) = svc_and_spy();
            spy.set_list_usage_records_response(RecordPage {
                items: vec![],
                next: None,
            });

            let err = svc
                .list_usage_records(
                    &ctx(),
                    meter_id(),
                    test_time_range(),
                    &query_ordered_by(&order),
                    &[],
                )
                .await
                .expect_err("an unsound keyset must be refused before dispatch");
            assert!(
                matches!(err, UsageCollectorError::InvalidArgument { .. }),
                "expected InvalidArgument for {order:?}, got {err:?}",
            );
            assert!(
                spy.last_list_order().is_none(),
                "a refused order MUST NOT reach the plugin: {order:?}",
            );
        }
    }

    /// A continuation request as the REST handler hands it to the service.
    ///
    /// Minted the way a conforming plugin does (`to_signed_tokens`) and
    /// then decoded the way `prepare_list_query` step 3 does
    /// (`ODataOrderBy::from_signed_tokens`), so the order under test is
    /// derived from the token rather than set alongside it.
    ///
    /// `f` is minted the same way: set to [`read_fingerprint`] over the
    /// caller's query and range, the value the gear itself would mint into
    /// `f` were this token built by a real first-page dispatch rather than
    /// by hand. Without it every fixture here would be refused as a cursor
    /// bound to another query, and these tests are about the order.
    fn cursor_request_ordered_by(keys: &[(&str, SortDir)]) -> ODataQuery {
        let signed = query_ordered_by(keys).order.to_signed_tokens();
        let mut query = ODataQuery::new();
        query.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        query.cursor = Some(CursorV1 {
            // Distinguishable per index, not one literal repeated: a
            // positional-alignment assertion cannot catch a transposition if
            // every value reads the same.
            k: (0..query.order.0.len())
                .map(|i| format!("boundary-{i}"))
                .collect(),
            o: query.order.0[0].dir,
            s: signed,
            f: Some(read_fingerprint(
                &meter_id(),
                test_time_range(),
                &query,
                &[],
            )),
            d: "fwd".to_owned(),
        });
        query
    }

    #[tokio::test]
    async fn a_conforming_continuation_reaches_the_plugin_with_the_order_it_was_minted_under() {
        // The service picks which mode of the floor to run, so this is where
        // "a continuation is checked, not extended" can fail: nothing stops
        // the service calling `establish_keyset_order` instead of
        // `require_continuation_keyset`. The plugin must receive the token's
        // order with the same keys, directions and width, which is what keeps
        // the continuation predicate aligned with its boundary values.
        let keys = [
            ("resource_id", SortDir::Desc),
            ("window_end", SortDir::Desc),
            ("id", SortDir::Desc),
        ];
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &cursor_request_ordered_by(&keys),
            &[],
        )
        .await
        .expect("a conforming continuation is a complete request");

        assert_eq!(
            received_order(&spy),
            expected(&keys),
            "a continuation MUST reach the plugin exactly as the token \
             bound it: a widened order no longer lines up with the \
             boundary values the token carries",
        );
        // The strip only does anything on a continuation; a first-page
        // dispatch carries no cursor to strip. The plugin must never see a
        // wire token — see [`crate::v2::domain::query`].
        assert!(
            spy.last_list_query()
                .expect("the plugin MUST have been dispatched")
                .cursor
                .is_none(),
            "the gateway must strip CursorV1 before dispatch on a \
             continuation, not only on a first page where there was never \
             one to strip",
        );
        let keyset = spy
            .last_list_keyset()
            .expect("a continuation carries boundary values the gateway must extract and hand on");
        assert_eq!(
            keyset.values(),
            (0..keys.len())
                .map(|i| format!("boundary-{i}"))
                .collect::<Vec<_>>(),
            "the plugin must receive exactly the token's own boundary \
             values, one per key and in the token's own order - \
             distinguishable per index, so a transposition would fail \
             this and not just a length check",
        );
        assert_eq!(
            keyset.direction(),
            SortDir::Desc,
            "the keyset's direction is the bound order's single direction",
        );
    }

    #[tokio::test]
    async fn a_continuation_the_floor_would_have_to_widen_never_reaches_the_plugin() {
        // On a cursor request the order must already be a sound keyset, not be
        // extended into one: appending `window_end` to a one-key token would
        // hand the plugin a two-key order against one boundary value — a
        // silently wrong page. Only a forged token or a non-conforming plugin
        // gets here.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &cursor_request_ordered_by(&[("resource_id", SortDir::Asc)]),
                &[],
            )
            .await
            .expect_err("a token bound to a non-unique keyset must be refused");
        match err {
            // The 400 blames the token, not an `$orderby` the caller cannot
            // send alongside a cursor — but the gear does not spell that
            // attribution or its wire code (Spec §3.13). Both are
            // `toolkit_odata`'s, derived from the error carried here, so
            // this pins the carried error and `infra::sdk_error_mapping`
            // pins the projection.
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::InvalidCursor),
                "expected upstream's InvalidCursor, got {source:?}",
            ),
            other => unreachable!("expected a cursor rejection, got {other:?}"),
        }
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    /// A `d: "bwd"` token with an otherwise-sound order, correct signed
    /// tokens and a matching fingerprint is refused on direction alone.
    ///
    /// `admit_continuation`'s other checks validate the *order* and never
    /// read `cursor.d`, and `CursorV1::decode` validates only that `d` is one
    /// of `{"fwd", "bwd"}`, not that it is the one this read path supports.
    /// The raw path is forward-only by design, so without
    /// `require_forward_cursor` such a token is served a forward page with
    /// `200`.
    #[tokio::test]
    async fn a_backward_cursor_never_reaches_the_plugin() {
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let keys = [("window_end", SortDir::Asc), ("id", SortDir::Asc)];
        let signed = query_ordered_by(&keys).order.to_signed_tokens();
        let mut query = ODataQuery::new();
        query.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        query.cursor = Some(CursorV1 {
            k: vec![
                "2026-01-01T00:00:00Z".to_owned(),
                "00000000-0000-0000-0000-000000000001".to_owned(),
            ],
            o: SortDir::Asc,
            s: signed,
            f: Some(read_fingerprint(
                &meter_id(),
                test_time_range(),
                &query,
                &[],
            )),
            // The one thing wrong with this token.
            d: "bwd".to_owned(),
        });

        let err = svc
            .list_usage_records(&ctx(), meter_id(), test_time_range(), &query, &[])
            .await
            .expect_err("a backward cursor must be refused: the raw path reads forward only");
        match err {
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::InvalidCursor),
                "expected upstream's InvalidCursor, got {source:?}",
            ),
            other => unreachable!("expected a cursor rejection, got {other:?}"),
        }
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }
}

/// The query a keyset continuation is bound to, enforced where both
/// surfaces pass through it.
///
/// `CursorV1::f` exists so a caller who changes their query between pages is
/// refused rather than served a continuation minted over a different row set.
/// Four inputs decide that row set: the caller's `$filter` and the three typed
/// parameters `gts_type_id`, the read range and `metadata_filter`. None
/// travels inside `$filter`, so each has to enter the fingerprint explicitly
/// — otherwise page 2 of a January query continues from a cursor minted over
/// February, and a wrong page is an `Ok`.
///
/// Asserted through the service rather than the handler: `filter_hash` is
/// `None` for an in-process caller and `validate_cursor_against` skips its
/// comparison when either side is `None`, so this is the caller an edge-only
/// check leaves unprotected.
mod read_path_cursor_fingerprint_tests {
    use std::sync::Arc;

    use std::collections::BTreeMap;

    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::v2::{
        IdempotencyKey, Keyset, MetadataFilter, MeterTypeId, RECORD_ID_FIELD, RecordOrigin,
        RecordPage, ResourceRef, TimeRange, UsageCollectorError, UsageCollectorPluginV2,
        UsageRecord, WINDOW_END_FIELD,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::list_query::read_fingerprint;
    use crate::v2::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, qty, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// Same shape as `read_path_keyset_floor_tests`.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        svc_and_spy_declaring(&[])
    }

    /// The same, over a declaration declaring exactly `declared` metadata
    /// keys: an undeclared key is refused by the query-surface gate long
    /// before the fingerprint is compared.
    fn svc_and_spy_declaring(declared: &[&str]) -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(declared))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// A second meter, so a cursor can be replayed against another one.
    /// `FakeDeclarationSource` resolves every id alike, so both are usable.
    fn other_meter_id() -> MeterTypeId {
        MeterTypeId::new(gts_id!(
            "cf.core.uc.usage_record.v2~cf.mini_chat._.other_meter.v1~"
        ))
        .expect("valid gts_type_id")
    }

    /// A [`MetadataFilter`] on `key` over `values`.
    fn filter_on(key: &str, values: &[&str]) -> MetadataFilter {
        MetadataFilter::new(key, values.iter().copied()).expect("valid metadata filter")
    }

    /// A range one hour wide, `offset` nanoseconds past the epoch.
    fn range_at(offset_nanos: i64) -> TimeRange {
        let from = time::OffsetDateTime::UNIX_EPOCH + time::Duration::nanoseconds(offset_nanos);
        TimeRange::new(from, from + time::Duration::hours(1)).expect("a one-hour range")
    }

    /// An [`ODataQuery`] whose `$filter` is the parsed `filter` string.
    fn query_with_filter(filter: &str) -> ODataQuery {
        let expr = toolkit_odata::parse_filter_string(filter)
            .expect("test filter parses")
            .into_expr();
        ODataQuery::from(Some(expr))
    }

    /// One persisted row, so the double's page can carry a continuation
    /// without tripping `query::verify_returned_keyset`'s empty-page guard.
    fn fixture_row(meter: &MeterTypeId) -> UsageRecord {
        UsageRecord {
            id: Uuid::from_u128(0xFEED),
            gts_type_id: meter.clone(),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: qty("1"),
            idempotency_key: IdempotencyKey::new("idem-mint-fingerprint")
                .expect("valid idempotency key"),
            accepted_at: OffsetDateTime::UNIX_EPOCH,
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    /// Page one of `query` over `range`, dispatched for real, returning the
    /// gateway's own read fingerprint as minted into the page's
    /// `next_cursor.f`.
    ///
    /// The mint leg of every round trip below. A real dispatch rather than a
    /// `read_fingerprint` call, so the value under test is the one the service
    /// actually minted. Read off the minted token, since
    /// `composed.filter_hash` is stripped to `None` before dispatch.
    async fn mint_fingerprint(
        meter: &MeterTypeId,
        range: TimeRange,
        query: &ODataQuery,
        metadata: &[MetadataFilter],
        declared: &[&str],
    ) -> String {
        let (svc, spy) = svc_and_spy_declaring(declared);
        spy.set_list_usage_records_response(RecordPage {
            items: vec![crate::v2::domain::test_support::as_stored(fixture_row(
                meter,
            ))],
            next: Some(
                Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc)
                    .expect("a non-empty, in-budget keyset"),
            ),
        });
        let page = svc
            .list_usage_records(&ctx(), meter.clone(), range, query, metadata)
            .await
            .expect("page one is a complete request");
        let token = page
            .page_info
            .next_cursor
            .as_deref()
            .expect("the double reports a further page");
        CursorV1::decode(token)
            .expect("the gateway mints a decodable token")
            .f
            .expect("every mint carries the gateway's own read fingerprint")
    }

    /// `query` again, carrying a continuation bound to `keys` verbatim — no
    /// flooring, so the order can be made deliberately unsound.
    ///
    /// The order round-trips through the signed tokens exactly as the handler
    /// rebuilds it, so what is under test is a decoded order rather than one
    /// set alongside the token.
    fn cursor_request_bound_to(query: &ODataQuery, keys: &[(&str, SortDir)]) -> ODataQuery {
        let signed = ODataOrderBy(
            keys.iter()
                .map(|(field, dir)| OrderKey {
                    field: (*field).to_owned(),
                    dir: *dir,
                })
                .collect(),
        )
        .to_signed_tokens();

        let mut out = query.clone();
        out.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        out.cursor = Some(CursorV1 {
            k: out.order.0.iter().map(|_| "boundary".to_owned()).collect(),
            o: out.order.0[0].dir,
            s: signed,
            f: None,
            d: "fwd".to_owned(),
        });
        out
    }

    fn continuation_of(query: &ODataQuery, fingerprint: &str) -> ODataQuery {
        let mut floored = query.clone();
        crate::v2::domain::list_query::establish_keyset_order(&mut floored)
            .expect("an empty order is floorable");
        let signed = floored.order.to_signed_tokens();

        let mut page_two = query.clone();
        page_two.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        page_two.cursor = Some(CursorV1 {
            k: page_two
                .order
                .0
                .iter()
                .map(|_| "boundary".to_owned())
                .collect(),
            o: page_two.order.0[0].dir,
            s: signed,
            f: Some(fingerprint.to_owned()),
            d: "fwd".to_owned(),
        });
        page_two
    }

    /// The 400 blames the token rather than an `$orderby` the caller cannot
    /// send alongside a cursor. The gear does not spell the wire code — that
    /// comes from the `toolkit_odata` error the refusal carries — so this pins
    /// the carried error and `infra::sdk_error_mapping` pins the projection.
    fn assert_query_mismatch(err: &UsageCollectorError) {
        match err {
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::FilterMismatch),
                "expected upstream's FilterMismatch, got {source:?}",
            ),
            other => unreachable!("expected a cursor rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_cursor_minted_under_the_same_range_is_served() {
        // The half a "just refuse every cursor" edit would break. The gateway
        // recomputes its fingerprint fresh on every dispatch, first page or
        // continuation alike, so an unchanged query mints page three's cursor
        // with the identical value page one minted page two's with.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![crate::v2::domain::test_support::as_stored(fixture_row(
                &meter_id(),
            ))],
            next: Some(
                Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc)
                    .expect("a non-empty, in-budget keyset"),
            ),
        });
        let page_two = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                range,
                &continuation_of(&caller, &fingerprint),
                &[],
            )
            .await
            .expect("page two of an unchanged query MUST be served");

        assert_eq!(
            spy.last_list_time_range(),
            Some(range),
            "the continuation must reach the plugin with the range it was \
             minted under",
        );
        let dispatched = spy
            .last_list_query()
            .expect("the plugin MUST have been dispatched");
        assert_eq!(
            dispatched
                .order
                .0
                .iter()
                .map(|key| (key.field.clone(), key.dir))
                .collect::<Vec<_>>(),
            vec![
                ("window_end".to_owned(), SortDir::Asc),
                ("id".to_owned(), SortDir::Asc),
            ],
            "and with the order rebuilt from the token's signed keys",
        );
        let token_three = page_two
            .page_info
            .next_cursor
            .as_deref()
            .expect("page two reports a further page");
        let minted_three =
            CursorV1::decode(token_three).expect("the gateway mints a decodable token");
        assert_eq!(
            minted_three.f,
            Some(fingerprint),
            "page three's minted cursor must carry the same fingerprint \
             page one's did: the gateway recomputes it fresh from the same \
             caller query and range on every dispatch, so a continuation \
             mints no differently than a first page would",
        );
    }

    #[tokio::test]
    async fn a_cursor_minted_under_a_different_range_never_reaches_the_plugin() {
        // The reason the range is in the fingerprint: same caller, same
        // `$filter`, a different range.
        let caller = query_with_filter("resource_id eq 'r1'");
        let january = test_time_range();
        let february = range_at(60 * 60 * 24 * 31 * 1_000_000_000);
        assert_ne!(january, february, "precondition: two different ranges");
        let fingerprint = mint_fingerprint(&meter_id(), january, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                february,
                &continuation_of(&caller, &fingerprint),
                &[],
            )
            .await
            .expect_err("a cursor minted over another range must be refused");

        assert_query_mismatch(&err);
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin: being served \
             is a wrong page, and a wrong page is an Ok",
        );
    }

    #[tokio::test]
    async fn a_cursor_differing_only_below_the_microsecond_is_rejected() {
        // The truncation hole `TimeRange::canonical_form` exists to avoid:
        // if the range rendering reused the identity derivation's fixed
        // six-digit-microsecond form, these two ranges would fingerprint
        // identically and this cursor would be served.
        let caller = query_with_filter("resource_id eq 'r1'");
        let coarse = range_at(0);
        let nudged = range_at(1);
        let fingerprint = mint_fingerprint(&meter_id(), coarse, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                nudged,
                &continuation_of(&caller, &fingerprint),
                &[],
            )
            .await
            .expect_err("a sub-microsecond range change must still be refused");

        assert_query_mismatch(&err);
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_cursor_minted_under_a_different_filter_never_reaches_the_plugin() {
        // An implementation that fingerprinted the range alone would lose
        // `$filter` silently while every range test above stayed green.
        let range = test_time_range();
        let fingerprint = mint_fingerprint(
            &meter_id(),
            range,
            &query_with_filter("resource_id eq 'r1'"),
            &[],
            &[],
        )
        .await;
        let page_two = query_with_filter("resource_id eq 'r2'");

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                range,
                &continuation_of(&page_two, &fingerprint),
                &[],
            )
            .await
            .expect_err("a cursor minted over another filter must be refused");

        assert_query_mismatch(&err);
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_cursor_carrying_no_fingerprint_never_reaches_the_plugin() {
        // `validate_cursor_against` skips its comparison whenever either side
        // is absent. The cursor is caller-supplied JSON, so admitting an
        // absent `f` leaves the whole check optional at the caller's
        // discretion.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let mut page_two = continuation_of(&caller, "unused");
        page_two.cursor.as_mut().expect("cursor").f = None;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &page_two, &[])
            .await
            .expect_err("an unbound cursor must be refused");

        assert_query_mismatch(&err);
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_cursor_minted_against_another_meter_never_reaches_the_plugin() {
        // `gts_type_id` is a typed parameter, so no filter hash covers it:
        // without it in the fingerprint a caller can continue a cursor against
        // a different meter and be served rows the token knows nothing about.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                other_meter_id(),
                range,
                &continuation_of(&caller, &fingerprint),
                &[],
            )
            .await
            .expect_err("a cursor minted against another meter must be refused");

        assert_query_mismatch(&err);
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    #[tokio::test]
    async fn a_cursor_minted_under_a_different_metadata_filter_never_reaches_the_plugin() {
        // The other typed row-selector, and one `$filter` cannot express: the
        // grammar has no surface for a dynamic JSON-map key, so a filter hash
        // reaches none of it.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let declared = ["region"];
        let fingerprint = mint_fingerprint(
            &meter_id(),
            range,
            &caller,
            &[filter_on("region", &["eu"])],
            &declared,
        )
        .await;

        let (svc, spy) = svc_and_spy_declaring(&declared);
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                range,
                &continuation_of(&caller, &fingerprint),
                &[filter_on("region", &["us"])],
            )
            .await
            .expect_err("a cursor minted under another metadata filter must be refused");

        assert_query_mismatch(&err);
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    #[tokio::test]
    async fn a_metadata_filter_respelled_the_same_way_still_paginates() {
        // The over-rejection guard: a REST caller's repeated `metadata.<key>`
        // parameters arrive in query-string order with duplicates intact, so
        // page two of an unchanged query can carry the same value set in a
        // different spelling. A fingerprint reading the slice verbatim would
        // refuse it.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let declared = ["region", "tier"];
        let fingerprint = mint_fingerprint(
            &meter_id(),
            range,
            &caller,
            &[
                filter_on("region", &["eu", "us"]),
                filter_on("tier", &["gold"]),
            ],
            &declared,
        )
        .await;

        let (svc, spy) = svc_and_spy_declaring(&declared);
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        svc.list_usage_records(
            &ctx(),
            meter_id(),
            range,
            &continuation_of(&caller, &fingerprint),
            // Entries swapped, values re-ordered, one value repeated — all
            // three are things a caller's own query string does.
            &[
                filter_on("tier", &["gold"]),
                filter_on("region", &["us", "eu", "us"]),
            ],
        )
        .await
        .expect("a re-spelled but identical metadata filter MUST still paginate");

        assert!(
            spy.last_list_order().is_some(),
            "the continuation MUST have reached the plugin",
        );
    }

    #[tokio::test]
    async fn an_in_process_order_diverging_from_its_token_never_reaches_the_plugin() {
        // The order the plugin sorts by must be the order the token's boundary
        // values were minted under. Each caller order below is itself a sound
        // keyset and the fingerprint matches, so no structural check sees the
        // divergence; left uncaught, the plugin compares a row-value tuple
        // against boundary values in another order — a misaligned
        // continuation served as an `Ok`. The handler overwrites `query.order`
        // from the token, so only an in-process caller can express this.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        // Permuted, narrower, and wider-with-a-different-lead.
        for caller_order in [
            vec![
                (RECORD_ID_FIELD, SortDir::Asc),
                (WINDOW_END_FIELD, SortDir::Asc),
            ],
            vec![(WINDOW_END_FIELD, SortDir::Asc)],
            vec![
                ("resource_id", SortDir::Asc),
                (WINDOW_END_FIELD, SortDir::Asc),
                (RECORD_ID_FIELD, SortDir::Asc),
            ],
        ] {
            // A conforming token, then the caller's contradictory order
            // laid over it.
            let mut divergent = continuation_of(&caller, &fingerprint);
            divergent.order = ODataOrderBy(
                caller_order
                    .iter()
                    .map(|(field, dir)| OrderKey {
                        field: (*field).to_owned(),
                        dir: *dir,
                    })
                    .collect(),
            );

            let (svc, spy) = svc_and_spy();
            spy.set_list_usage_records_response(RecordPage {
                items: vec![],
                next: None,
            });
            svc.list_usage_records(&ctx(), meter_id(), range, &divergent, &[])
                .await
                .expect("the token itself is conforming, so this must be served");

            assert_eq!(
                spy.last_list_order()
                    .expect("the plugin MUST have been dispatched"),
                vec![
                    (WINDOW_END_FIELD.to_owned(), SortDir::Asc),
                    (RECORD_ID_FIELD.to_owned(), SortDir::Asc),
                ],
                "the plugin MUST sort by the order the TOKEN bound, never \
                 the caller's {caller_order:?}: a row-value tuple compared \
                 against boundary values in another order is a silently \
                 wrong page",
            );
        }
    }

    #[tokio::test]
    async fn an_in_process_token_whose_signed_keys_do_not_decode_never_reaches_the_plugin() {
        // A shape only the binding can catch: the token's `s` is malformed so
        // no order can be derived from it, but the caller supplied a sound
        // `order` of their own that a structural check would read instead.
        // The handler rejects this at the edge, so only an in-process caller
        // can express it.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let mut malformed = continuation_of(&caller, &fingerprint);
        malformed.cursor.as_mut().expect("cursor").s = String::new();

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &malformed, &[])
            .await
            .expect_err("a token whose signed keys do not decode must be refused");

        match err {
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::InvalidCursor),
                "expected upstream's InvalidCursor, got {source:?}",
            ),
            other => unreachable!("expected a cursor rejection, got {other:?}"),
        }
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_sound_caller_order_cannot_launder_an_unsound_token() {
        // Why the binding runs BEFORE the structural check. The token is bound
        // to `+resource_id` alone, which is no keyset; the caller supplies the
        // canonical order beside it. Check-then-bind would validate the
        // caller's sound order and then overwrite it with the token's unsound
        // one, handing the plugin the very order the check exists to refuse.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let mut laundered = cursor_request_bound_to(&caller, &[("resource_id", SortDir::Asc)]);
        laundered.cursor.as_mut().expect("cursor").f = Some(fingerprint);
        laundered.order = ODataOrderBy(vec![
            OrderKey {
                field: WINDOW_END_FIELD.to_owned(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: RECORD_ID_FIELD.to_owned(),
                dir: SortDir::Asc,
            },
        ]);

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &laundered, &[])
            .await
            .expect_err("an unsound token must be refused whatever order accompanies it");

        assert!(matches!(err, UsageCollectorError::CursorRejected { .. }));
        assert!(
            spy.last_list_order().is_none(),
            "a sound caller order MUST NOT launder an unsound token past the check",
        );
    }

    #[tokio::test]
    async fn a_doubly_defective_cursor_is_refused_for_its_order_first() {
        // `require_continuation_keyset` checks structure before relevance, and
        // the order is caller-visible: a token BOTH bound to an unsound keyset
        // and minted over another query carries `InvalidCursor`, and would
        // carry `FilterMismatch` if the two checks were swapped — a different
        // wire reason code and `error_category`. Every other cursor test is
        // defective in exactly one way, so none of them distinguishes this.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();

        // Bound to a one-key order (not a keyset: it names neither
        // canonical field) AND carrying a fingerprint from another range.
        let stale = mint_fingerprint(
            &meter_id(),
            range_at(60 * 60 * 24 * 1_000_000_000),
            &caller,
            &[],
            &[],
        )
        .await;
        let mut doubly_defective =
            cursor_request_bound_to(&caller, &[("resource_id", SortDir::Asc)]);
        doubly_defective.cursor.as_mut().expect("cursor").f = Some(stale);

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &doubly_defective, &[])
            .await
            .expect_err("a doubly-defective token must be refused");

        match err {
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::InvalidCursor),
                "the ORDER defect must be reported: it is checked first, and \
                 a swap would silently re-label this as FilterMismatch; got \
                 {source:?}",
            ),
            other => unreachable!("expected a cursor rejection, got {other:?}"),
        }
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn the_dispatched_fingerprint_is_computed_from_the_callers_filter_not_the_composed_one() {
        // `compose_query_with_scope` AND-merges the server-injected PDP scope
        // into `$filter` and preserves the caller's `filter_hash`: the scope
        // is not caller-controlled, so a fingerprint over the composed filter
        // embeds a value the next request's recomputation cannot reproduce.
        // The round trip is stable either way here (the PDP fake returns the
        // same scope every call), so this asserts the value itself.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![crate::v2::domain::test_support::as_stored(fixture_row(
                &meter_id(),
            ))],
            next: Some(
                Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc)
                    .expect("a non-empty, in-budget keyset"),
            ),
        });
        let page = svc
            .list_usage_records(&ctx(), meter_id(), range, &caller, &[])
            .await
            .expect("page one is a complete request");

        let dispatched = spy
            .last_list_query()
            .expect("the plugin MUST have been dispatched");
        assert_ne!(
            toolkit_odata::short_filter_hash(dispatched.filter()),
            toolkit_odata::short_filter_hash(caller.filter()),
            "precondition: the PDP scope must actually have narrowed the \
             composed $filter, or this test cannot tell the two apart",
        );
        let token = page
            .page_info
            .next_cursor
            .as_deref()
            .expect("page one reports a further page");
        let minted = CursorV1::decode(token).expect("the gateway mints a decodable token");
        assert_eq!(
            minted.f,
            Some(read_fingerprint(&meter_id(), range, &caller, &[])),
            "the fingerprint the gateway mints MUST be computed from the \
             caller's filter and range, never the PDP-composed filter",
        );
    }
}

// ── The gateway owns the raw-path cursor ───────────────────
//
// `cpt-cf-usage-collector-dod-gateway-owned-cursor` requires the plugin
// receive "a structured keyset of the last row's sort values" and never mint,
// encode or interpret a wire token. Before the reshape the plugin minted the
// token and the gear passed it through; this module is the oracle for the
// reversed obligation, on both edges of one dispatch: what the plugin is
// handed (no cursor, no keyset on a first page) and what the caller gets back
// (a token the gear minted from the plugin's keyset).
mod gateway_owned_cursor_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use parking_lot::Mutex;
    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataQuery, SortDir};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::v2::{
        IdempotencyKey, Keyset, MetadataFilter, MeterRef, MeterTypeId, RecordOrigin, RecordPage,
        ResourceRef, TimeRange, UsageCollectorPluginError, UsageCollectorPluginV2, UsageRecord,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        ServiceFixture, authenticated_ctx, fake_declaration_source_with_metadata, qty,
        recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// What one `list_usage_records` dispatch handed [`ListCallRecorder`].
    // No `Default`: `ast::Expr` has none, and a recorded call is only ever
    // built from a real dispatch.
    #[derive(Debug, Clone)]
    struct RecordedListCall {
        /// Whether `query.cursor` was `None` on this dispatch: the plugin
        /// must never see a wire token ([`crate::v2::domain::query`]).
        cursor_was_none: bool,
        /// `query.filter_hash` as dispatched. The SPI doc says the slot
        /// carries no guarantee on this method and MUST NOT be read, so a
        /// conforming dispatch strips it to `None` on every call.
        filter_hash: Option<String>,
        /// The `keyset` parameter this dispatch was handed.
        keyset: Option<Keyset>,
        /// The `scope` parameter this dispatch was handed: the caller's
        /// compiled PDP scope, named rather than folded away.
        scope: toolkit_odata::ast::Expr,
        /// `query.filter` as dispatched, which must still carry the scope
        /// AND-merged into it.
        filter: Option<toolkit_odata::ast::Expr>,
    }

    /// A `list_usage_records`-only double; every other SPI method fails
    /// loudly. It records what it was handed and always answers a fixed page
    /// carrying a continuation, so one call shows both what the plugin
    /// received and what the gateway minted from what it returned.
    #[derive(Default)]
    struct ListCallRecorder {
        last_call: Mutex<Option<RecordedListCall>>,
    }

    impl ListCallRecorder {
        fn last_call(&self) -> Option<RecordedListCall> {
            self.last_call.lock().clone()
        }

        /// The boundary values of the fixed [`Keyset`] this double's page
        /// always carries.
        fn returned_keyset_values() -> Vec<String> {
            vec![
                "2026-01-01T00:00:00Z".to_owned(),
                Uuid::from_u128(0xF00D).to_string(),
            ]
        }

        /// The one row this double's page always carries. A page carrying a
        /// continuation keyset MUST NOT be empty
        /// (`query::verify_returned_keyset`): a continuation names the last
        /// row of the page it continues.
        fn returned_row() -> UsageRecord {
            UsageRecord {
                id: Uuid::from_u128(0xF00D),
                gts_type_id: meter_id(),
                tenant_id: Uuid::from_u128(1),
                resource_ref: ResourceRef::new("rsc", "compute.vm").expect("valid resource ref"),
                subject_ref: None,
                metadata: BTreeMap::new(),
                quantity: qty("1"),
                idempotency_key: IdempotencyKey::new("idem-list-call-recorder")
                    .expect("valid idempotency key"),
                accepted_at: OffsetDateTime::UNIX_EPOCH,
                origin: RecordOrigin::Live,
                invalidation: None,
                window_start: OffsetDateTime::UNIX_EPOCH,
                window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            }
        }
    }

    #[async_trait]
    impl UsageCollectorPluginV2 for ListCallRecorder {
        async fn create_usage_records(
            &self,
            _records: Vec<usage_collector_sdk::v2::MeteredRecord>,
        ) -> Result<
            Vec<Result<usage_collector_sdk::v2::StoredUsageRecord, UsageCollectorPluginError>>,
            UsageCollectorPluginError,
        > {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: create_usage_records must not be called",
            ))
        }

        async fn query_aggregated_usage_records(
            &self,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _fold: usage_collector_sdk::v2::AggregationFold,
            _query: &ODataQuery,
            _scope: &toolkit_odata::ast::Expr,
            _metadata_filter: &[MetadataFilter],
            _group_by: &[usage_collector_sdk::v2::AggregationDimension],
        ) -> Result<usage_collector_sdk::v2::AggregationResult, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: query_aggregated_usage_records must not be called",
            ))
        }

        async fn list_usage_records(
            &self,
            _meter: &MeterRef,
            _time_range: TimeRange,
            query: &ODataQuery,
            scope: &toolkit_odata::ast::Expr,
            _metadata_filter: &[MetadataFilter],
            keyset: Option<&Keyset>,
        ) -> Result<RecordPage, UsageCollectorPluginError> {
            *self.last_call.lock() = Some(RecordedListCall {
                cursor_was_none: query.cursor.is_none(),
                filter_hash: query.filter_hash.clone(),
                keyset: keyset.cloned(),
                scope: scope.clone(),
                filter: query.filter().cloned(),
            });
            Ok(RecordPage {
                items: vec![crate::v2::domain::test_support::as_stored(
                    Self::returned_row(),
                )],
                next: Some(
                    Keyset::new(Self::returned_keyset_values(), SortDir::Asc)
                        .expect("a non-empty, in-budget keyset"),
                ),
            })
        }

        async fn get_usage_record(
            &self,
            _id: Uuid,
            _scope: &toolkit_odata::ast::Expr,
            _converged_only: bool,
        ) -> Result<usage_collector_sdk::v2::StoredUsageRecord, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: get_usage_record must not be called",
            ))
        }

        async fn read_feed_page(
            &self,
            _subscription: &usage_collector_sdk::v2::SubscribedMeters,
            _scope: &toolkit_odata::ast::Expr,
            _start: usage_collector_sdk::v2::FeedStart<usage_collector_sdk::v2::FeedPosition>,
            _until: Option<usage_collector_sdk::v2::FeedPosition>,
            _limit: u64,
        ) -> Result<
            usage_collector_sdk::v2::FeedPage<
                usage_collector_sdk::v2::FeedPosition,
                usage_collector_sdk::v2::StoredUsageRecord,
            >,
            UsageCollectorPluginError,
        > {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: read_feed_page must not be called",
            ))
        }

        async fn get_reconciliation_metadata(
            &self,
            _tenant_id: Uuid,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _fold: usage_collector_sdk::v2::AggregationFold,
            _scope: &toolkit_odata::ast::Expr,
        ) -> Result<usage_collector_sdk::v2::ReconciliationMetadata, UsageCollectorPluginError>
        {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: get_reconciliation_metadata must not be called",
            ))
        }
    }

    fn service_with_list_recorder(plugin: Arc<ListCallRecorder>) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(
                plugin as Arc<dyn UsageCollectorPluginV2>,
                "test.usage_collector.list.gateway_owned_cursor.v1",
            )
    }

    /// The compiled PDP scope reaches the plugin as a named argument, and is
    /// **also** still AND-merged into `query.filter`.
    ///
    /// Both halves matter. Before the argument existed, the scope arrived
    /// folded into `query.filter` and nothing in the signature or the doc said
    /// so, so a plugin author who implemented the stated contract and never
    /// read `query.filter` served every tenant's rows while meeting every
    /// obligation the SPI named. Keeping the fold is what makes either read
    /// scope-correct rather than moving the whole guarantee onto one argument
    /// a plugin may still ignore.
    #[tokio::test]
    async fn the_compiled_scope_reaches_the_plugin_named_and_still_folded_into_the_filter() {
        let recorder = Arc::new(ListCallRecorder::default());
        let svc = service_with_list_recorder(Arc::clone(&recorder));

        // Both arms of the composition, because they fail differently: with no
        // caller filter the scope *is* the composed filter, while with one it
        // has to survive as a conjunct beside the caller's predicate. Only the
        // second arm catches a composition that stops AND-ing and keeps the
        // caller's half.
        for query in [
            ODataQuery::new(),
            super::query_with_filter("resource_id eq 'r1'"),
        ] {
            svc.list_usage_records(&ctx(), meter_id(), test_time_range(), &query, &[])
                .await
                .expect("a first page is served");

            let call = recorder.last_call().expect("the plugin was dispatched");

            let filter = call
                .filter
                .expect("composition always sets a filter; the scope narrows every read");
            assert!(
                format!("{filter:?}").contains(&format!("{:?}", call.scope)),
                "the scope must still be AND-merged into query.filter, so a plugin \
                 reading the filter alone is scope-correct; scope was {:?} and the \
                 dispatched filter was {filter:?}",
                call.scope,
            );
        }
    }

    /// The gateway mints the raw page's continuation, and the plugin never
    /// sees a wire cursor — it receives a structured keyset of the last row's
    /// sort values instead. See [`crate::v2::domain::query`].
    #[tokio::test]
    async fn the_gateway_mints_the_continuation_and_the_plugin_receives_a_keyset() {
        let recorder = Arc::new(ListCallRecorder::default());
        let svc = service_with_list_recorder(Arc::clone(&recorder));

        let page = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect("a first page is served");

        // What the plugin was handed: no wire cursor, no filter_hash, and
        // no keyset on a first page.
        let call = recorder.last_call().expect("the plugin was dispatched");
        assert!(
            call.cursor_was_none,
            "the gateway must strip CursorV1 before dispatch; the plugin saw one"
        );
        assert!(
            call.filter_hash.is_none(),
            "the gateway must strip filter_hash before dispatch (the same \
             MUST-NOT as the cursor); the plugin saw {:?}",
            call.filter_hash
        );
        assert!(
            call.keyset.is_none(),
            "a first page carries no seek key; got {:?}",
            call.keyset
        );

        // What the caller got: a token the gear minted, bound to the order
        // the gear dispatched and carrying the gear's own fingerprint.
        let token = page
            .page_info
            .next_cursor
            .as_deref()
            .expect("the recorder's page reports a further page");
        let minted = CursorV1::decode(token).expect("the gateway mints a decodable token");
        assert_eq!(
            minted.k,
            ListCallRecorder::returned_keyset_values(),
            "the token's boundary values are the keyset the plugin returned"
        );
        assert!(
            minted.f.is_some(),
            "the minted token carries the gear's read fingerprint"
        );
        assert_eq!(minted.d, "fwd", "the raw path reads forward only");
        // `s` is what `bind_continuation_order` rebuilds the follow-up
        // dispatch's order from; a wrong `s` is a silently wrong page.
        assert_eq!(
            minted.s, "+window_end,+id",
            "the token must bind to the order the gateway actually \
             dispatched under (the floored canonical default, since this \
             call named none), not an order recomputed from the caller's \
             own unfloored request",
        );
        assert_eq!(
            minted.o,
            SortDir::Asc,
            "the token's recorded direction is the keyset's own direction",
        );
    }

    /// A minted token, fed back as the next request's cursor, reaches the
    /// plugin with exactly the keyset it returned — the mint→decode→seek seam
    /// closed end to end. The pg walk tests drive `Option<&Keyset>` directly,
    /// the other gear tests feed hand-built `CursorV1`s, and the test above
    /// mints a token it never re-dispatches; this is the one that walks a page
    /// boundary through the wire token as a real two-page read does.
    #[tokio::test]
    async fn a_minted_token_fed_back_reaches_the_plugin_with_the_keyset_it_returned() {
        let recorder = Arc::new(ListCallRecorder::default());
        let svc = service_with_list_recorder(Arc::clone(&recorder));

        let page_one = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect("a first page is served");
        let token = page_one
            .page_info
            .next_cursor
            .clone()
            .expect("the recorder's page reports a further page");

        let mut continuation = ODataQuery::new();
        continuation.cursor =
            Some(CursorV1::decode(&token).expect("the gateway mints a decodable token"));

        svc.list_usage_records(&ctx(), meter_id(), test_time_range(), &continuation, &[])
            .await
            .expect("the gateway's own minted token is honoured on its own follow-up");

        let call = recorder
            .last_call()
            .expect("the plugin was dispatched on the follow-up");
        assert!(
            call.cursor_was_none,
            "the gateway must strip the wire cursor on every paged call, \
             the continuation included - not only on a first page where \
             there was never one to strip",
        );
        assert!(
            call.filter_hash.is_none(),
            "the gateway must strip filter_hash on every paged call too \
             - the plugin saw {:?}",
            call.filter_hash
        );
        let received = call
            .keyset
            .expect("a continuation carries the decoded keyset");
        let expected = Keyset::new(ListCallRecorder::returned_keyset_values(), SortDir::Asc)
            .expect("a non-empty, in-budget keyset");
        assert_eq!(
            received, expected,
            "the keyset the plugin receives on the follow-up must equal the \
             one it returned on the page that minted this token - `Keyset`'s \
             `PartialEq` is exactly this comparison, and anything else means \
             the mint, the encode/decode round trip, or the extraction back \
             out dropped or corrupted a value",
        );
    }
}

/// The gear mints the raw path's `next_cursor` from whatever [`Keyset`] a
/// plugin hands back on [`usage_collector_sdk::v2::RecordPage::next`], so
/// `Service::list_usage_records` is the only place to catch one that cannot
/// be bound: the wrong number of boundary values, a direction disagreeing with
/// the dispatched order, or a token on a page with no rows to continue from.
/// Each is a host-contract breach rather than caller input, on the distinction
/// [`crate::v2::domain::Service::read_usage_feed`] draws for the feed's
/// over-length page.
mod returned_keyset_verification_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataQuery, SortDir};
    use usage_collector_sdk::v2::{
        IdempotencyKey, Keyset, MetadataFilter, MeterTypeId, RecordOrigin, RecordPage, ResourceRef,
        UsageCollectorError, UsageRecord,
    };
    use uuid::Uuid;

    use crate::v2::domain::Service;
    use crate::v2::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, qty, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn meter() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    /// One persisted row, so a page that is meant to look non-empty does.
    fn sample_record() -> UsageRecord {
        UsageRecord {
            id: Uuid::from_u128(0xBEEF),
            gts_type_id: meter(),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: qty("1"),
            idempotency_key: IdempotencyKey::new("idem-returned-keyset")
                .expect("valid idempotency key"),
            accepted_at: OffsetDateTime::UNIX_EPOCH,
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    /// A `Service` whose plugin answers a one-row page carrying `keyset` as
    /// its continuation. `region` is declared so the metadata-filter test
    /// below can narrow on it.
    fn service_with_list_keyset(keyset: Keyset) -> Arc<Service> {
        let plugin = RecordingPlugin::new();
        plugin.set_list_usage_records_response(RecordPage {
            items: vec![crate::v2::domain::test_support::as_stored(sample_record())],
            next: Some(keyset),
        });
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&["region"]))
            .with_resolver(recording_plugin_resolver())
            .build(plugin, RECORDING_PLUGIN_SUFFIX)
    }

    /// The same, over a declaration declaring every one of `keys` — a wider
    /// `metadata_filter` surface than [`service_with_list_keyset`]'s lone
    /// `region`.
    fn service_with_list_keyset_declaring(keyset: Keyset, keys: &[&str]) -> Arc<Service> {
        let plugin = RecordingPlugin::new();
        plugin.set_list_usage_records_response(RecordPage {
            items: vec![crate::v2::domain::test_support::as_stored(sample_record())],
            next: Some(keyset),
        });
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(keys))
            .with_resolver(recording_plugin_resolver())
            .build(plugin, RECORDING_PLUGIN_SUFFIX)
    }

    /// A `Service` whose plugin answers an EMPTY page that still carries
    /// `keyset`.
    fn service_with_empty_page_and_keyset(keyset: Keyset) -> Arc<Service> {
        let plugin = RecordingPlugin::new();
        plugin.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: Some(keyset),
        });
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(plugin, RECORDING_PLUGIN_SUFFIX)
    }

    /// A keyset of the wrong width is a host-contract breach, not a page: a
    /// plugin returning three values for a two-key order would mint a token
    /// whose boundary values cannot line up with the order it binds.
    /// `Internal`, not `InvalidArgument` — no request the caller can send
    /// avoids it.
    #[tokio::test]
    async fn a_returned_keyset_of_the_wrong_arity_is_refused_as_internal() {
        let svc = service_with_list_keyset(
            Keyset::new(["a", "b", "c"], SortDir::Asc).expect("three values"),
        );
        let err = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect_err("a three-value keyset against a two-key order is a breach");
        let UsageCollectorError::Internal { ref detail } = err else {
            unreachable!("expected Internal, got {err:?}");
        };
        assert!(
            detail.contains('3') && detail.contains('2'),
            "the detail must name both widths so an operator can see which \
             side is wrong; got {detail}"
        );
    }

    /// The same for the direction, which the arity check cannot catch.
    #[tokio::test]
    async fn a_returned_keyset_sorting_against_the_dispatched_order_is_refused() {
        // The gear dispatches Asc: no caller order, so the appended pair.
        let svc = service_with_list_keyset(
            Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Desc).expect("two values"),
        );
        let err = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect_err("a Desc keyset against an Asc order is a breach");
        assert!(
            matches!(err, UsageCollectorError::Internal { .. }),
            "got {err:?}"
        );
    }

    /// An empty page carrying a continuation. Arity and direction both check
    /// out, so neither guard above catches it, and following the token yields
    /// another empty page with another token — a hang rather than a wrong
    /// value.
    #[tokio::test]
    async fn an_empty_page_carrying_a_continuation_is_refused() {
        let svc = service_with_empty_page_and_keyset(
            Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc).expect("two values"),
        );
        let err = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect_err("a continuation with no rows to continue from is a breach");
        let UsageCollectorError::Internal { ref detail } = err else {
            unreachable!("expected Internal, got {err:?}");
        };
        assert!(
            detail.contains("empty"),
            "the detail must say the page was empty, which is the fact an \
             operator acts on; got {detail}"
        );
    }

    /// The gear mints from its own `read_fingerprint`, so the fingerprint
    /// must still bind all three typed parameters: a caller who changes
    /// `metadata_filter` between pages is refused, not served a page computed
    /// under a different selection.
    #[tokio::test]
    async fn a_cursor_is_refused_when_the_metadata_filter_changed_between_pages() {
        let svc = service_with_list_keyset(
            Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc).expect("two values"),
        );
        let first = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect("a first page is served");
        let token = first
            .page_info
            .next_cursor
            .as_deref()
            .expect("the double reports a further page");

        let mut resumed = ODataQuery::new();
        resumed.cursor = Some(CursorV1::decode(token).expect("decodable"));
        let narrowed = [MetadataFilter::new("region", ["eu"]).expect("a valid filter")];

        let err = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &resumed,
                &narrowed,
            )
            .await
            .expect_err("page two under a different metadata_filter is a different query");
        assert!(
            matches!(err, UsageCollectorError::CursorRejected { .. }),
            "a changed typed parameter is a filter mismatch on the cursor, \
             not a 500; got {err:?}"
        );
    }

    /// A minted cursor stays inside the published `maxLength` at the widest
    /// filter set **and** the widest keyset the surface admits: 16 predicates
    /// of 32 values each plus the widest admissible `$orderby`, with the
    /// keyset's own contribution bounded separately by `MAX_KEYSET_BYTES`.
    ///
    /// Both axes sit at their actual worst case, not merely their arity: the
    /// keyset below uses `MAX_KEYSET_BYTES`'s own per-field worst-case values
    /// (`keyset.rs`, derived in `keyset_tests.rs`) rather than same-length
    /// placeholders — the caller-supplied string fields are wider than the
    /// rest, so an arity-only widening leaves the composed worst case
    /// unpinned.
    #[tokio::test]
    async fn the_widest_admissible_request_mints_a_cursor_inside_the_published_bound() {
        let values: Vec<String> = (0..crate::v2::domain::query::MAX_METADATA_FILTER_VALUES)
            .map(|i| format!("value-{i:04}"))
            .collect();
        let keys: Vec<String> = (0..crate::v2::domain::query::MAX_METADATA_FILTERS)
            .map(|i| format!("key-{i:04}"))
            .collect();
        let filters: Vec<MetadataFilter> = keys
            .iter()
            .map(|key| MetadataFilter::new(key.clone(), values.clone()).expect("valid"))
            .collect();
        let declared_keys: Vec<&str> = keys.iter().map(String::as_str).collect();

        let order = toolkit_odata::ODataOrderBy(
            usage_collector_sdk::v2::KEYSET_SAFE_RECORD_FIELDS
                .iter()
                .map(|f| toolkit_odata::OrderKey {
                    field: (*f).to_owned(),
                    dir: SortDir::Asc,
                })
                .collect(),
        );

        let four_byte_char = '\u{1F600}'; // 😀 — 1 char, 4 UTF-8 bytes, no JSON escaping needed
        let attribution_at_cap: String = std::iter::repeat_n(four_byte_char, 256).collect();
        let uuid_width = "00000000-0000-0000-0000-000000000001".to_owned();
        let rfc3339_at_width = "2023-11-14T23:13:20.123456789Z".to_owned();
        let origin_worst = "backfill".to_owned();
        let worst_case_keyset_values: Vec<String> =
            usage_collector_sdk::v2::KEYSET_SAFE_RECORD_FIELDS
                .iter()
                .map(|field| match *field {
                    "resource_id" | "resource_type" => attribution_at_cap.clone(),
                    "id" | "tenant_id" => uuid_width.clone(),
                    "window_start" | "window_end" | "accepted_at" => rfc3339_at_width.clone(),
                    "origin" => origin_worst.clone(),
                    other => unreachable!(
                        "KEYSET_SAFE_RECORD_FIELDS grew a field (`{other}`) this test does not \
                     yet assign a worst-case value for; classify it above rather than \
                     silently narrowing the keyset axis back to an arity-only test"
                    ),
                })
                .collect();
        assert_eq!(
            serde_json::to_string(&worst_case_keyset_values)
                .expect("Vec<String> always serializes")
                .len(),
            usage_collector_sdk::v2::MAX_KEYSET_BYTES,
            "precondition: this keyset must sit at the published bound's actual worst case, \
             not merely at some value inside it, or this test proves nothing about the bound",
        );

        let svc = service_with_list_keyset_declaring(
            Keyset::new(worst_case_keyset_values, SortDir::Asc)
                .expect("exactly MAX_KEYSET_BYTES is inside the bound (inclusive)"),
            &declared_keys,
        );

        let page = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new().with_order(order),
                &filters,
            )
            .await
            .expect("the widest admissible request is served");

        let token = page
            .page_info
            .next_cursor
            .as_deref()
            .expect("the double reports a further page");
        assert!(
            token.len() <= crate::v2::domain::feed::MAX_CURSOR_TOKEN_CHARS,
            "a minted cursor must stay inside the published maxLength; the widest \
             admissible request produced {} bytes against a bound of {}",
            token.len(),
            crate::v2::domain::feed::MAX_CURSOR_TOKEN_CHARS,
        );
    }
}

// The query-surface gate is stated in [`crate::v2::domain::query`]; its
// pure-function coverage lives in `query_tests.rs`. This module proves the
// checks are wired into `Service::list_usage_records` /
// `Service::query_aggregated_usage_records` — right position (before
// dispatch), right arguments (the resolved declaration's `declared_keys`).
mod query_admissibility_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{ODataQuery, ast};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::v2::{
        MetadataFilter, MeterTypeId, RecordPage, UsageCollectorError, UsageCollectorPluginV2,
        ValidationReason,
    };

    use crate::v2::domain::Service;
    use crate::v2::domain::ports::declarations::DeclarationSource;
    use crate::v2::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_not_found, fake_declaration_source_with_metadata,
        recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// An `ODataQuery` whose `$filter` is a bare `reserved_field eq 'x'`
    /// predicate, so whatever the request is rejected for is the
    /// admissibility gate under test. The mandatory read range travels beside
    /// the filter as a typed parameter and contributes no conjunct.
    fn filter_naming(reserved_field: &str) -> ODataQuery {
        ODataQuery::from(Some(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier(reserved_field.to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String("x".to_owned()))),
        )))
    }

    /// An `ODataQuery` whose `$filter` is `field eq '<value>'`, with the
    /// literal under the caller's control — `filter_naming` fixes it at
    /// `"x"`, which cannot express an off-label closed-set literal.
    fn filter_eq(field: &str, value: &str) -> ODataQuery {
        ODataQuery::from(Some(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier(field.to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String(value.to_owned()))),
        )))
    }

    /// A `Service` + [`RecordingPlugin`] spy over `source`, wired against the
    /// fixed-tenant PDP fake both read paths require.
    fn service_with_recording_plugin(
        source: Arc<dyn DeclarationSource>,
    ) -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(source)
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// Assert `err` is the canonical reserved-filter-field rejection.
    fn assert_reserved_field_rejection(err: &UsageCollectorError) {
        match err {
            UsageCollectorError::InvalidArgument { field, reason, .. } => {
                assert_eq!(field, "$filter", "attributes to $filter");
                assert_eq!(*reason, ValidationReason::Validation);
            }
            other => unreachable!("expected InvalidArgument/Validation, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn list_rejects_a_filter_naming_gts_type_id() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &filter_naming("gts_type_id"),
                &[],
            )
            .await
            .expect_err("a $filter naming gts_type_id must be rejected");
        assert_reserved_field_rejection(&err);
        // `RecordingPlugin::calls()` counts `query_aggregated_usage_records`
        // dispatches only; the list path's "did it dispatch" oracle is
        // `last_list_time_range`.
        assert!(
            spy.last_list_time_range().is_none(),
            "a rejected filter must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn list_rejects_a_filter_naming_a_covered_period_bound() {
        // Both bounds are filterable-schema fields — they have to be for the
        // `(window_end, id)` keyset to resolve to a column — so the
        // reserved-field guard is the only thing between a `$filter`
        // predicate and the plugin. The case-varied spellings are here
        // because `FilterField::from_name` compares with
        // `eq_ignore_ascii_case`, so `WINDOW_END` folds to `WindowEnd` rather
        // than dead-ending as an unknown field; the guard matches that.
        for field in ["window_start", "window_end", "WINDOW_END", "Window_Start"] {
            let (svc, spy) =
                service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

            let err = svc
                .list_usage_records(
                    &ctx(),
                    meter_id(),
                    test_time_range(),
                    &filter_naming(field),
                    &[],
                )
                .await
                .expect_err("a $filter naming a covered-period bound must be rejected");
            assert_reserved_field_rejection(&err);
            assert!(
                spy.last_list_time_range().is_none(),
                "a rejected filter must never reach the plugin: {field}",
            );
        }
    }

    #[tokio::test]
    async fn list_accepts_a_filter_naming_no_reserved_field() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await
        .expect("no reserved field named: the plugin must be reached");
    }
    // ── metadata_filter admissibility ─────────────────────────────────────
    //
    // `metadata_filter` is the dynamic-key side channel that exists because
    // `toolkit-odata`'s grammar cannot express a filter over a JSON map key.
    // It never flows through `$filter`, so it needs its own declared-keys
    // gate on both read paths.

    /// Build a `MetadataFilter` for `key` with a single candidate value.
    fn metadata_filter(key: &str) -> MetadataFilter {
        MetadataFilter::new(key, ["x".to_owned()]).expect("valid metadata filter")
    }

    #[tokio::test]
    async fn list_rejects_an_undeclared_metadata_filter_key() {
        let (svc, _spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));

        let filters = [metadata_filter("tier")];
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &filters,
            )
            .await
            .expect_err("an undeclared metadata_filter key must be rejected");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
    }

    #[tokio::test]
    async fn list_accepts_a_declared_metadata_filter_key() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let filters = [metadata_filter("region")];
        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &filters,
        )
        .await
        .expect("a declared metadata_filter key must be accepted, reaching the plugin");
    }
    #[tokio::test]
    async fn metadata_filter_admissibility_is_recomputed_per_request() {
        // The `group_by` recomputation proof mirrored onto `metadata_filter`,
        // on the list path.
        let filters = [metadata_filter("region")];

        let (svc_before, _spy_before) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        let err = svc_before
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &filters,
            )
            .await
            .expect_err("not yet declared: must be rejected");
        assert!(err.to_string().contains("region"));

        let (svc_after, spy_after) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy_after.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        svc_after
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &filters,
            )
            .await
            .expect("declared a moment later: must now be admissible, without a restart");
    }
    #[tokio::test]
    async fn list_now_fails_closed_when_the_type_does_not_resolve() {
        // Gating `metadata_filter` against the declared keys requires
        // resolving a declaration, so an unresolvable type fails closed as a
        // pre-dispatch 404 — the list-path mirror of
        // `aggregate_fails_closed_when_the_type_does_not_resolve`.
        let (svc, _spy) = service_with_recording_plugin(fake_declaration_source_not_found());

        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
            )
            .await
            .expect_err("an unresolvable type must not reach the plugin");
        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
    }
    // ── Closed-label `$filter` literals ───────────────────────────────────
    //
    // `entry_type` and `origin` are closed-label columns: an off-label
    // `entry_type` literal raises PostgreSQL 22P02 at the backend (a 500 on
    // caller input), and an off-label `origin` literal silently answers an
    // empty page. `query::reject_off_label_literals` refuses both before
    // dispatch, on both read paths. The property under test here is "before
    // dispatch", which only the plugin-double call count can show; the
    // function's own return value is pinned in `query_tests.rs`.

    #[tokio::test]
    async fn list_rejects_an_off_label_entry_type_literal_before_dispatch() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &filter_eq("entry_type", "Record"),
                &[],
            )
            .await
            .expect_err("`Record` is not a label");
        let UsageCollectorError::InvalidArgument { ref detail, .. } = err else {
            unreachable!("expected InvalidArgument, got {err:?}");
        };
        assert!(
            detail.contains("Record"),
            "the refusal must echo the offending value; got {detail}"
        );
        // `last_list_time_range` is the list path's "did it dispatch" oracle;
        // `calls()` counts aggregate dispatches only.
        assert!(
            spy.last_list_time_range().is_none(),
            "an off-label literal must never reach the plugin"
        );
    }
    #[tokio::test]
    async fn list_accepts_a_filter_naming_a_label_in_the_closed_set() {
        // The positive direction: a label the set contains must still reach
        // the plugin, so the guard discriminates the value rather than
        // refusing `entry_type` / `origin` outright.
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &filter_eq("entry_type", "record"),
            &[],
        )
        .await
        .expect("`record` is an admissible label");
        assert!(
            spy.last_list_time_range().is_some(),
            "an admissible label must reach the plugin"
        );
    }
}
