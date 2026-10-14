//! Unit tests for the two ledger read statements and the page assembly. Pure
//! (no DB) throughout: `build_get_sql`, `build_list_sql` and
//! `build_list_page` are the whole of what `get` and `list` decide, so
//! everything they owe is asserted without a backend.
//!
//! Two obligations have no compiler backstop and no visible effect until a
//! second page, which is why they are pinned here rather than left to an
//! integration run: that the compiled PDP scope is the whole filter beyond the
//! `id` (a dropped scope is an authorization bypass that still returns rows),
//! and that the keyset tuple and the `ORDER BY` are built from one order.
//!
//! That the methods *propagate* a refusal rather than reading without it is
//! `super::super::record_store`'s own test: it needs a pool to discriminate
//! against.

// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};
use usage_collector_sdk::v2::{Keyset, USAGE_RECORD_BASE_TYPE, UsageCollectorPluginError};

use super::{build_get_sql, build_list_page, build_list_sql, record_row_key};
use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::query::translate::{SqlBind, record_column};
use crate::infra::storage::test_support::{
    RANGE_FROM_UNIX, RANGE_TO_UNIX, READ_FINGERPRINT, SCOPE_TENANT_A, SCOPE_TENANT_B,
    WINDOW_END_UNIX, WINDOW_START_UNIX, list_meter, list_query, list_range, parse_scope,
    row_matching, unit_meter, unit_record,
};

// --- The point lookup intersects the compiled scope -----------------
//
// The SPI hands `get` the caller's compiled PDP scope and no caller `$filter`,
// so the scope is the whole filter beyond the `id`. Nothing above the SPI
// re-reads the row that comes back, so everything below tests the only remaining
// place "exists but not yours reads as NotFound" is kept.

/// Everything a built statement says after its `FROM`, so an expectation can be
/// written out by hand without restating [`RECORD_COLUMNS`] — which has its own,
/// independent, oracle elsewhere.
///
/// For the point lookup that is the `WHERE` alone; for the keyset page it also
/// carries the table alias, the `ORDER BY` and the `LIMIT`, which is what lets
/// one hand-transcribed string pin that the keyset tuple and the `ORDER BY` read
/// the same order.
fn statement_tail(sql: &str) -> String {
    sql.split_once(" FROM usage_records ")
        .unwrap_or_else(|| panic!("a built statement must read `usage_records`. got: {sql}"))
        .1
        .to_owned()
}

#[test]
fn the_point_lookup_renders_the_scope_as_its_whole_statement_tail() {
    let scope = parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}"));

    let (sql, _binds) = build_get_sql(&scope).expect("a scope must render");

    assert!(
        sql.contains("WHERE id = $1 AND ("),
        "the point lookup carries no caller filter, so the compiled scope is \
         the whole filter the row must satisfy; a lookup that selects on id \
         alone is an existence oracle. got: {sql}"
    );
}

#[test]
fn the_point_lookup_binds_the_scope_values_after_the_id() {
    // Two conjuncts of different value types, so a dropped or reordered bind
    // shows up as a different variant rather than a different uuid.
    let scope = parse_scope(&format!(
        "tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm'"
    ));

    let (sql, binds) = build_get_sql(&scope).expect("a scope must render");

    // The `id` owns `$1`, so the scope's own binds start at `$2`. If they
    // started at `$1` the tenant predicate would read the id bind and the
    // statement would carry one more parameter than it names.
    assert_eq!(
        statement_tail(&sql),
        "WHERE id = $1 AND ((tenant_id = $2 AND resource_type = $3))",
        "the scope's binds follow the id, which occupies $1"
    );
    assert_eq!(
        binds.len(),
        2,
        "both scope operands must be bound, in placeholder order. got: {binds:?}"
    );
    assert!(
        matches!(&binds[0], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_A),
        "$2 is the tenant the scope pins. got: {:?}",
        binds[0]
    );
    assert!(
        matches!(&binds[1], SqlBind::Str(s) if s == "vm"),
        "$3 is the resource type the scope pins. got: {:?}",
        binds[1]
    );
}

#[test]
fn the_point_lookups_disjunctive_scope_survives_the_conjunction_with_the_id() {
    // The shape a multi-constraint grant compiles to through
    // `authz::scope_to_odata_filter`: a disjunction of tenant-pinned
    // conjunctions. Conjoined without parentheses of its own,
    // `id = $1 AND A OR B` binds as `(id = $1 AND A) OR B` and answers every row
    // matching the last disjunct. The wrap that prevents that is
    // `translate_scope`'s, not this builder's.
    let scope = parse_scope(&format!(
        "(tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm') or \
         (tenant_id eq {SCOPE_TENANT_B} and resource_type eq 'vm')"
    ));

    let (sql, binds) = build_get_sql(&scope).expect("a scope must render");

    // Transcribed by hand, not derived from anything the builder produces.
    assert_eq!(
        statement_tail(&sql),
        "WHERE id = $1 AND (((tenant_id = $2 AND resource_type = $3) \
         OR (tenant_id = $4 AND resource_type = $5)))",
        "every disjunct of the scope has to survive the conjunction with the id"
    );
    assert_eq!(binds.len(), 4, "four operands, four binds");
}

#[test]
fn the_point_lookup_carries_no_invalidation_predicate() {
    // The fold excludes a withdrawn pair; this path is the ledger and MUST
    // return one as persisted. The scope below names neither column, so anything
    // matching in the predicate was added by the query builder. The select list
    // is exempt and has to be: `invalidates` is one of the columns a withdrawn
    // entry is read back *through*.
    let scope = parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}"));

    // This covers the SQL half only. The other way to withhold a withdrawn entry
    // is to read the row and then drop it — `if row.invalidates.is_some() {
    // return NotFound }` in `get`'s `Some(row)` arm — which no unit test here can
    // see, because none executes a statement.
    let (sql, _binds) = build_get_sql(&scope).expect("a scope must render");
    let predicate = statement_tail(&sql);

    assert!(
        !predicate.contains("invalidates"),
        "a withdrawn entry MUST be returned as persisted here; an `invalidates` \
         predicate on the ledger path destroys the audit trail the append-only \
         model exists to keep, and the exclusion belongs to the fold. \
         got: {predicate}"
    );
    assert!(
        !predicate.contains("entry_type"),
        "restricting the point lookup to `entry_type = 'record'` withholds the \
         invalidation half of a withdrawn pair, which is the same data loss by \
         another spelling. got: {predicate}"
    );
}

#[test]
fn the_point_lookup_renders_a_membership_scope_over_several_tenants() {
    // `scope_to_odata_filter` pins the owning tenant with `Eq` *or* `In`, so a
    // multi-tenant grant reaches the plugin as a membership test. Every other
    // test here only ever hands it `Eq`.
    let scope = parse_scope(&format!(
        "tenant_id in ({SCOPE_TENANT_A}, {SCOPE_TENANT_B})"
    ));

    let (sql, binds) = build_get_sql(&scope).expect("a scope must render");

    assert_eq!(
        statement_tail(&sql),
        "WHERE id = $1 AND (tenant_id IN ($2, $3))",
        "a membership scope narrows the lookup to the tenants it names"
    );
    assert_eq!(binds.len(), 2, "one bind per named tenant");
}

#[test]
fn a_scope_naming_a_field_off_the_allowlist_is_refused() {
    // `gts_type_uuid` is a typed SPI parameter, deliberately absent from the
    // filterable schema and from `record_column`. A scope naming it must be an
    // error: dropping the unrenderable conjunct would leave `WHERE id = $1`,
    // turning a translation failure into an authorization bypass.
    let scope = parse_scope(&format!("gts_type_uuid eq '{USAGE_RECORD_BASE_TYPE}'"));

    let Err(err) = build_get_sql(&scope) else {
        panic!("a scope naming a field off the allowlist must not render");
    };

    assert!(
        err.contains("gts_type_uuid"),
        "the refusal names the field it refused. got: {err}"
    );
}

// --- The cursor key on the row ------------------------------------
//
// `record_row_key` is the inverse of `record_column` on the pagination path: one
// resolves an order field to the column the `ORDER BY` and the keyset tuple are
// rendered from, the other to the boundary value the minted cursor carries.
// Nothing in the type system couples them, so they can disagree silently —
// `record_column` resolving a field `record_row_key` answers `None` for is a
// `500` at the mint and no compile error.

/// A row whose every keyset-safe column carries a value distinguishable from
/// every other one, so an arm reading a neighbouring column produces a visibly
/// wrong string rather than a plausible one.
///
/// `subject_id` and `subject_type` are deliberately *present*: a `None` from
/// `record_row_key` on either has to be "not a keyset key", not "the column was
/// NULL".
fn keyed_row() -> UsageRecordRow {
    UsageRecordRow {
        id: uuid::Uuid::from_u128(0xA1),
        tenant_id: uuid::Uuid::from_u128(0xB2),
        gts_type_uuid: unit_meter().uuid,
        type_key: 1,
        quantity: rust_decimal::Decimal::new(7, 0),
        window_start: time::OffsetDateTime::from_unix_timestamp(WINDOW_START_UNIX)
            .expect("valid ts"),
        window_end: time::OffsetDateTime::from_unix_timestamp(WINDOW_END_UNIX).expect("valid ts"),
        resource_id: "res-keyed".to_owned(),
        resource_type: "compute.vm".to_owned(),
        subject_id: Some("subject-keyed".to_owned()),
        subject_type: Some("user".to_owned()),
        idempotency_key: "idem-keyed".to_owned(),
        invalidates: None,
        reason_code: None,
        origin: "backfill".to_owned(),
        entry_type: "record".to_owned(),
        // Distinct from `window_end`: `accepted_at` joined the keyset-safe
        // vocabulary alongside it, and this row's whole point is that every
        // keyset-safe column reads back a value distinguishable from every
        // other one's.
        accepted_at: time::OffsetDateTime::from_unix_timestamp(WINDOW_END_UNIX + 120)
            .expect("valid ts"),
        metadata: serde_json::json!({}),
    }
}

/// Every keyset-safe order field paired with the boundary value
/// [`keyed_row`] must yield for it, transcribed by hand rather than read back
/// out of the row.
///
/// The field names are asserted against the SDK's own list below, so this
/// pairing cannot quietly cover a subset of it.
const KEYED_ROW_BOUNDARIES: [(&str, &str); 8] = [
    ("id", "00000000-0000-0000-0000-0000000000a1"),
    ("window_start", "2023-11-14T22:13:20Z"),
    ("window_end", "2023-11-14T23:13:20Z"),
    ("tenant_id", "00000000-0000-0000-0000-0000000000b2"),
    ("resource_id", "res-keyed"),
    ("resource_type", "compute.vm"),
    ("origin", "backfill"),
    ("accepted_at", "2023-11-14T23:15:20Z"),
];

#[test]
fn every_keyset_safe_order_field_has_a_cursor_key_on_the_row() {
    // Driven off the SDK's list rather than a local copy, so a field added
    // there fails here — at build time, on a named arm — instead of at the
    // mint, in production, on whichever caller first ordered by it.
    let row = keyed_row();

    for field in usage_collector_sdk::v2::KEYSET_SAFE_RECORD_FIELDS {
        assert!(
            record_row_key(&row, field).is_some(),
            "`{field}` is an admissible $orderby key, so a page ending on this \
             row has to be able to mint a boundary for it"
        );
    }
}

#[test]
fn each_cursor_key_reads_the_column_its_order_field_names() {
    // The half an `is_some()` sweep cannot see. Every arm returns *a* string
    // either way; only a named value tells `window_end` reading `window_start`
    // apart from `window_end` reading `window_end`, and that mis-pointing
    // renders a boundary the `ORDER BY` never sorted by — the page resumes
    // somewhere else and reports nothing.
    let row = keyed_row();

    let named: Vec<&str> = KEYED_ROW_BOUNDARIES
        .iter()
        .map(|(field, _)| *field)
        .collect();
    assert_eq!(
        named.as_slice(),
        usage_collector_sdk::v2::KEYSET_SAFE_RECORD_FIELDS,
        "the hand-written pairing has to cover the SDK's whole keyset vocabulary"
    );

    for (field, expected) in KEYED_ROW_BOUNDARIES {
        assert_eq!(
            record_row_key(&row, field).as_deref(),
            Some(expected),
            "`{field}` must read the column `record_column` resolves it to"
        );
    }
}

#[test]
fn a_field_that_is_not_a_keyset_key_mints_no_boundary() {
    // Each of these resolves through `record_column`, so each can appear in an
    // `ORDER BY` that renders, and none is keyset-safe. The domain-optional ones
    // would let a row-value tuple compare as NULL and drop rows out of the page
    // silently; `entry_type` is `NOT NULL` in the ledger and is refused for the
    // other reason `record_row_key`'s doc gives. `subject_id` / `subject_type` are
    // populated on this row so the refusal cannot be read as a NULL column.
    let row = keyed_row();

    for field in ["subject_id", "subject_type", "invalidates", "entry_type"] {
        assert!(
            record_column(field).is_some(),
            "the premise of this test: `{field}` is an allowlisted column"
        );
        assert!(
            !usage_collector_sdk::v2::is_keyset_safe_record_field(field),
            "the premise of this test: `{field}` is not keyset-safe"
        );
        assert!(
            record_row_key(&row, field).is_none(),
            "`{field}` is not a keyset key, so it must not seed a boundary"
        );
    }
}

// --- The keyset page ----------------------------------------------
//
// `build_list_sql` and `build_list_page` are the two pure halves of `list`; what
// is left in `list` is the round trip between them, so everything below is
// asserted on those two without a database. That matters because the fingerprint
// obligation has no compiler backstop and no visible effect until page two.

/// One stored row, distinct by `id`, mappable back to the SDK model.
fn list_row(seq: u128) -> UsageRecordRow {
    row_matching(
        &unit_record(uuid::Uuid::from_u128(0xD0), &format!("idem-{seq}"), seq),
        serde_json::json!({}),
    )
}

#[test]
fn selection_reads_the_period_end_alone() {
    // `from <= window_end < to`
    // (`cpt-cf-usage-collector-adr-window-end-selection`). Not overlap, which
    // selects one entry into two adjacent ranges, and not containment, which
    // drops it out of both — a covered period longer than the range is what
    // tells the three apart. The predicate therefore never reads
    // `window_start`, and the negative half of this assertion is the half that
    // says so.
    let query = list_query();

    let (sql, binds) = build_list_sql(list_meter(), list_range(), &query, &[], None, 25)
        .expect("the canonical first page must render");
    let tail = statement_tail(&sql);

    assert!(
        tail.contains("r.window_end >= $2"),
        "the lower bound is inclusive on the period end. got: {tail}"
    );
    assert!(
        tail.contains("r.window_end < $3"),
        "the upper bound is exclusive on the period end. got: {tail}"
    );
    assert!(
        !tail.contains("window_start"),
        "the time-range predicate must not read the period start: selecting on \
         it fails window-end-selection and quantity-round-trip alike, the \
         latter because its read-back range is one second wide at the period \
         end while the period began an hour earlier. got: {tail}"
    );
    // The clause text above pins the predicate; these pin what is bound into it.
    // Without them the bounds can be swapped (inverting the range, which selects
    // nothing ever), bound from one end twice, or the meter bound as an empty
    // string — none of which changes a character of the SQL.
    assert_eq!(binds.len(), 3, "the meter and the two range bounds");
    assert!(
        matches!(&binds[0], SqlBind::Uuid(u) if *u == list_meter()),
        "$1 is the meter this page reads. got: {:?}",
        binds[0]
    );
    assert!(
        matches!(&binds[1], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_FROM_UNIX),
        "$2 is the range's inclusive lower bound. got: {:?}",
        binds[1]
    );
    assert!(
        matches!(&binds[2], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_TO_UNIX),
        "$3 is the range's exclusive upper bound. got: {:?}",
        binds[2]
    );
}

#[test]
fn a_look_ahead_page_returns_the_last_rows_keyset_for_the_gateway_to_mint_from() {
    // The plugin mints nothing: `cpt-cf-usage-collector-dod-gateway-owned-cursor`
    // puts minting in the gateway, and this method hands back the last in-page
    // row's typed `Keyset` instead of an encoded token.
    //
    // Two rows for a page of one: the look-ahead row is what makes a keyset get
    // built at all.
    let query = list_query();

    let page = build_list_page(vec![list_row(1), list_row(2)], &query, 1)
        .expect("a look-ahead page must assemble");

    assert_eq!(
        page.items.len(),
        1,
        "the look-ahead row is dropped, not served"
    );
    let keyset = page
        .next
        .expect("a look-ahead row means a next page's keyset");
    assert_eq!(
        keyset.values(),
        [
            "2023-11-14T23:13:20Z",
            "00000000-0000-0000-0000-000000000001"
        ],
        "one boundary value per key of the canonical (window_end, id) order, \
         read off the last in-page row"
    );
    assert_eq!(
        keyset.direction(),
        SortDir::Asc,
        "the keyset's direction is the dispatched order's single direction"
    );
}

#[test]
fn a_keyset_that_will_not_fit_the_wire_cursor_is_refused_rather_than_built() {
    // `Keyset::new` enforces `MAX_KEYSET_BYTES` on the JSON-encoded values, and
    // `build_list_page` must surface that refusal as `Internal` rather than
    // construct a keyset the gateway could never mint into a token
    // (`usage_collector_sdk::v2::MAX_KEYSET_BYTES`'s own doc: two admissible order
    // keys, `resource_id` and `resource_type`, are caller-supplied strings of
    // no fixed length).
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![OrderKey {
            field: "resource_id".to_owned(),
            dir: SortDir::Asc,
        }]))
        .with_filter_hash(READ_FINGERPRINT.to_owned());
    let mut overlong_row = list_row(1);
    overlong_row.resource_id = "x".repeat(usage_collector_sdk::v2::MAX_KEYSET_BYTES + 1);

    let err = build_list_page(vec![overlong_row, list_row(2)], &query, 1)
        .expect_err("a keyset this large must not be built");

    match err {
        UsageCollectorPluginError::Internal(msg) => assert!(
            msg.contains("exceeds"),
            "the refusal names what the keyset violated. got: {msg}"
        ),
        other => panic!("expected an Internal error, got {other:?}"),
    }
}

#[test]
fn a_mixed_direction_order_refuses_the_keyset_rather_than_pick_the_leading_key() {
    // `build_list_page` resolves direction through `uniform_dir`, not off
    // `query.order`'s leading key: that alone would build a `Keyset::direction()`
    // unable to describe this order (`id` sorts one way and `window_end` the
    // other), and the gateway would mint a continuation as though the whole order
    // agreed. The gateway guarantees uniform direction on every surface, but this
    // function cannot rely on `render_order_by` happening to run first.
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: "id".to_owned(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: "window_end".to_owned(),
                dir: SortDir::Desc,
            },
        ]))
        .with_filter_hash(READ_FINGERPRINT.to_owned());

    let err = build_list_page(vec![list_row(1), list_row(2)], &query, 1)
        .expect_err("a mixed-direction order must not yield a keyset");

    match err {
        UsageCollectorPluginError::Internal(msg) => assert!(
            msg.contains("mixed-direction"),
            "the refusal names the defect. got: {msg}"
        ),
        other => panic!("expected an Internal error, got {other:?}"),
    }
}

#[test]
fn a_page_that_did_not_fill_builds_no_keyset() {
    // No look-ahead row, so there is nothing after this page and a keyset would
    // invite the gateway to mint a continuation to an empty one.
    let query = list_query();

    let page = build_list_page(vec![list_row(1)], &query, 25).expect("a short page must assemble");

    assert_eq!(page.items.len(), 1);
    assert!(page.next.is_none());
}

#[test]
fn an_exactly_full_page_builds_no_keyset() {
    // The boundary case the look-ahead comparison decides, and the only one that
    // tells `>` from `>=`. A page holding exactly `limit` rows had no look-ahead
    // row, so there is nothing after it. Under `>=` it reads as overfull: the last
    // row is truncated away and a keyset built from the row before it.
    let query = list_query();

    let page =
        build_list_page(vec![list_row(1)], &query, 1).expect("an exactly-full page must assemble");

    assert_eq!(
        page.items.len(),
        1,
        "an exactly-full page serves every row it read"
    );
    assert!(
        page.next.is_none(),
        "no look-ahead row was read, so nothing follows this page"
    );
}

#[test]
fn the_built_keyset_is_read_in_the_order_it_was_handed() {
    // `window_end` and `id` are guaranteed present in the order, not guaranteed
    // last: a caller ordering by `id` is handed on as `(id, window_end)`. The
    // keys are read in that order, so a build that assumed a canonical slot
    // would transpose them and the continuation would compare a uuid against a
    // timestamptz.
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: "id".to_owned(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: "window_end".to_owned(),
                dir: SortDir::Asc,
            },
        ]))
        .with_filter_hash(READ_FINGERPRINT.to_owned());

    // Three rows for a page of two, so the page's last row is not also its
    // first: with two rows at a limit of one the two coincide and a keyset
    // built from `first()` reads exactly like one built from `last()`. The
    // boundary has to be the row the caller last saw, or the continuation
    // re-serves rows this page already delivered.
    let page = build_list_page(vec![list_row(1), list_row(2), list_row(3)], &query, 2)
        .expect("an id-led page must assemble");

    assert_eq!(
        page.items
            .iter()
            .map(|r| r.id.to_string())
            .collect::<Vec<_>>(),
        vec![
            "00000000-0000-0000-0000-000000000001".to_owned(),
            "00000000-0000-0000-0000-000000000002".to_owned(),
        ],
        "the look-ahead row is dropped, and the page serves the rest in the \
         order it read them, so the boundary below is the key of the last row \
         the caller actually saw. Counting the items instead leaves the two \
         unrelated: reverse them and every continuation re-serves or skips \
         rows while the boundary assertion goes on passing"
    );
    let keyset = page
        .next
        .expect("a look-ahead row means a next page's keyset");

    // Transcribed by hand: `list_row(2)`'s id — the last row of the page, not
    // its first — then the covered-period end every fixture shares.
    assert_eq!(
        keyset.values(),
        [
            "00000000-0000-0000-0000-000000000002".to_owned(),
            "2023-11-14T23:13:20Z".to_owned(),
        ],
        "one key per order field, in the order's own field order, read off the \
         last row of the page"
    );
    assert_eq!(
        keyset.direction(),
        SortDir::Asc,
        "the keyset's direction is that order's single direction"
    );
}

#[test]
fn the_composed_filter_survives_the_conjunction_with_the_range() {
    // What arrives in `query.filter` is the caller's filter `And`-composed with
    // the compiled PDP scope, or — as here — the scope alone, which for a
    // multi-constraint grant puts an `Or` of tenant-pinned conjunctions at the
    // outermost node. Pushed into the `AND` join unparenthesized,
    // `… AND A OR B` binds as `(… AND A) OR B` and answers every row matching the
    // last disjunct, across every tenant the range covers. It also pins the half
    // nothing structural does: that the fragment is pushed at all.
    let query = list_query().with_filter(parse_scope(&format!(
        "(tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm') or \
         (tenant_id eq {SCOPE_TENANT_B} and resource_type eq 'vm')"
    )));

    let (sql, binds) = build_list_sql(list_meter(), list_range(), &query, &[], None, 25)
        .expect("a disjunctive composed filter must render");

    // Transcribed by hand, not derived from anything the builder produces.
    assert_eq!(
        statement_tail(&sql),
        "r WHERE r.gts_type_uuid = $1 \
         AND r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_uuid = $1) \
         AND r.window_end >= $2 AND r.window_end < $3 \
         AND (((tenant_id = $4 AND resource_type = $5) \
         OR (tenant_id = $6 AND resource_type = $7))) \
         ORDER BY window_end ASC, id ASC LIMIT 26",
        "every disjunct has to survive the conjunction with the meter and the range"
    );
    assert_eq!(
        binds.len(),
        7,
        "the meter, the two bounds, and four operands"
    );
}

#[test]
fn a_composed_filter_that_cannot_be_translated_is_refused_never_dropped() {
    // `gts_type_uuid` is a typed SPI parameter, deliberately absent from the
    // filterable schema. Dropping the unrenderable conjunct would leave the
    // meter and the range as the whole `WHERE` — a translation failure turned
    // into an authorization bypass across every tenant in that range.
    let query = list_query().with_filter(parse_scope(&format!(
        "gts_type_uuid eq '{USAGE_RECORD_BASE_TYPE}'"
    )));

    let Err(err) = build_list_sql(list_meter(), list_range(), &query, &[], None, 25) else {
        panic!("a filter naming a field off the schema must not render");
    };

    assert!(
        err.contains("gts_type_uuid"),
        "the refusal names the field it refused. got: {err}"
    );
}

#[test]
fn the_keyset_tuple_and_the_order_by_read_one_order() {
    // Non-canonical in both respects the SPI warns about: `id` leads, and the
    // direction is descending. `render_order_by` and `keyset_predicate` consume
    // `query.order` separately, and disagreeing on field order or direction resumes
    // the page from the wrong boundary silently — so one hand-written string pins
    // the tuple, its comparison operator and the `ORDER BY` together.
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: "id".to_owned(),
                dir: SortDir::Desc,
            },
            OrderKey {
                field: "window_end".to_owned(),
                dir: SortDir::Desc,
            },
        ]))
        .with_filter_hash(READ_FINGERPRINT.to_owned());
    let keyset = Keyset::new(
        [
            "00000000-0000-0000-0000-000000000001".to_owned(),
            "2023-11-14T23:13:20Z".to_owned(),
        ],
        SortDir::Desc,
    )
    .expect("a non-empty, in-budget keyset");

    let (sql, binds) = build_list_sql(list_meter(), list_range(), &query, &[], Some(&keyset), 25)
        .expect("a descending id-led continuation must render");

    // Transcribed by hand.
    assert_eq!(
        statement_tail(&sql),
        "r WHERE r.gts_type_uuid = $1 \
         AND r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_uuid = $1) \
         AND r.window_end >= $2 AND r.window_end < $3 \
         AND (id, window_end) < ($4, $5) \
         ORDER BY id DESC, window_end DESC LIMIT 26",
        "the tuple's columns, its operator and the ORDER BY all read one order"
    );
    assert_eq!(
        binds.len(),
        5,
        "the meter, the two bounds, and two cursor keys"
    );
}

#[test]
fn the_raw_list_statement_names_no_offset_on_either_page_shape() {
    // `inst-seek-no-offset` / `cpt-cf-uc-plugin-dod-no-filter-widening-no-offset`:
    // the resumption on this path is a row-value key comparison, never a row count
    // to walk past. Pinned over the one function that builds this path's statement,
    // for both page shapes it builds — a first page (`keyset: None`) and a
    // continuation (`keyset: Some`) — so neither can grow an `OFFSET` unnoticed.
    let first_page_query = list_query();
    let (first_page_sql, _) =
        build_list_sql(list_meter(), list_range(), &first_page_query, &[], None, 25)
            .expect("a first page must render");
    assert!(
        !first_page_sql.to_uppercase().contains("OFFSET"),
        "a first-page raw-list statement must contain no OFFSET clause. got: {first_page_sql}"
    );

    let continuation_query = list_query();
    let keyset = Keyset::new(
        [
            "2023-11-14T23:13:20Z".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
        ],
        SortDir::Asc,
    )
    .expect("a non-empty, in-budget keyset");
    let (continuation_sql, _) = build_list_sql(
        list_meter(),
        list_range(),
        &continuation_query,
        &[],
        Some(&keyset),
        25,
    )
    .expect("a continuation must render");
    assert!(
        !continuation_sql.to_uppercase().contains("OFFSET"),
        "a continuation raw-list statement must contain no OFFSET clause. got: {continuation_sql}"
    );
}

#[test]
fn the_metadata_side_channel_is_bound_after_the_range() {
    // AND across filters, OR within one filter's values. Asserted here because
    // the side channel shares its bind context with the range and the filter,
    // so a builder that seeded it wrongly would renumber every placeholder
    // after the third.
    let query = list_query();
    let filters =
        [
            usage_collector_sdk::v2::MetadataFilter::new("region", ["eu-west-1", "eu-west-2"])
                .expect("valid metadata filter"),
        ];

    let (sql, binds) = build_list_sql(list_meter(), list_range(), &query, &filters, None, 25)
        .expect("a metadata-filtered page must render");

    assert_eq!(
        statement_tail(&sql),
        "r WHERE r.gts_type_uuid = $1 \
         AND r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_uuid = $1) \
         AND r.window_end >= $2 AND r.window_end < $3 \
         AND r.metadata ->> $4 IN ($5, $6) \
         ORDER BY window_end ASC, id ASC LIMIT 26"
    );
    assert_eq!(
        binds.len(),
        6,
        "the meter, the two bounds, the key and two values"
    );
}

#[test]
fn the_ledger_page_withholds_no_withdrawn_entry() {
    // The asymmetry with the fold, as a test rather than only a comment. A total
    // that counts a withdrawn entry is wrong, so `aggregate` excludes the pair —
    // but that is a derived view and this is the ledger itself: "a withdrawn pair
    // MUST likewise be returned as persisted here". Hiding either half destroys
    // the audit trail (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    let query = list_query();

    let (sql, _) = build_list_sql(list_meter(), list_range(), &query, &[], None, 25)
        .expect("the canonical first page must render");
    let tail = statement_tail(&sql);

    assert!(
        !tail.contains("invalidates"),
        "no withdrawal exclusion belongs on this path. got: {tail}"
    );
    assert!(
        !tail.contains("entry_type"),
        "nor an entry-type restriction, which withholds the same rows by \
         another name. got: {tail}"
    );
}

#[test]
fn a_keyset_field_off_the_keyset_safe_allowlist_is_refused() {
    // Non-empty, in-budget, and not naming a keyset-safe field — exercised
    // purely for `keyset_predicate`'s own field-allowlist refusal, which
    // `build_list_sql` must still propagate once the guard above it (the
    // gateway-owned arity/direction/order checks) is out of this
    // function's hands.
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![OrderKey {
            field: "subject_id".to_owned(),
            dir: SortDir::Asc,
        }]))
        .with_filter_hash(READ_FINGERPRINT.to_owned());
    let keyset = Keyset::new(["sub-1".to_owned()], SortDir::Asc).expect("a non-empty keyset");

    let Err(err) = build_list_sql(list_meter(), list_range(), &query, &[], Some(&keyset), 25)
    else {
        panic!("a keyset field off the keyset-safe allowlist must not render");
    };

    assert!(
        err.contains("subject_id"),
        "the refusal names the field it refused. got: {err}"
    );
}

#[test]
fn an_empty_order_is_refused_rather_than_served_unpaginated() {
    // A gateway breach: the SPI guarantees `query.order` is non-empty on every
    // surface, because it is the keyset the continuation is built from. Served
    // with no `ORDER BY`, the pages would overlap and drop rows against a
    // backend free to return them in any order.
    let query = ODataQuery::new().with_filter_hash(READ_FINGERPRINT.to_owned());

    let Err(err) = build_list_sql(list_meter(), list_range(), &query, &[], None, 25) else {
        panic!("an empty order must be refused");
    };

    assert!(err.contains("empty"), "unexpected message: {err}");
}
