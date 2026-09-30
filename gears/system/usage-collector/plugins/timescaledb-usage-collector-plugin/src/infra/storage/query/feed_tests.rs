// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use toolkit_odata::ast;
use uuid::Uuid;

use super::*;

/// A scope naming one tenant, which is the shape the host compiles.
///
/// `toolkit_odata::ast::Expr` is a tuple-variant enum (`Compare(Box<Expr>,
/// CompareOperator, Box<Expr>)`, `Identifier(String)`), and `CompareOperator`
/// carries `Eq` (`libs/toolkit-odata/src/lib.rs`'s `ast` module) — not the
/// struct-variant, `Field`/`CompareOp::Eq` shape this task's brief sketched.
/// `tests/common/mod.rs::tenant_scope` is the same shape, and is what this
/// mirrors.
fn tenant_scope(tenant: Uuid) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(tenant))),
    )
}

#[test]
fn a_first_read_places_no_lower_bound_on_position() {
    // Ruling D1, and the plugin's DESIGN §3.6 First read: "There is no start
    // lookup and no age threshold." A first read must carry no position
    // predicate at all — not a band, not a floor, not an age.
    let (sql, _binds) =
        build_feed_page_sql(None, None, &tenant_scope(Uuid::nil()), 10).expect("the scope renders");
    assert!(
        !sql.contains("(xact_id, id) >"),
        "a first read begins at the oldest entry the subscription retains: {sql}"
    );
    assert!(
        sql.contains("xact_id < ($2)::xid8"),
        "the settled horizon still bounds it: {sql}"
    );
    assert!(
        sql.contains("ORDER BY xact_id, id"),
        "the feed order is the pair: {sql}"
    );
    assert!(sql.ends_with("LIMIT 10"), "the limit is rendered: {sql}");
}

#[test]
fn the_page_statement_reads_the_feed_column_list_not_the_plain_one() {
    // Task 3 of this slice pointed this builder at `RECORD_COLUMNS` because
    // the split had not landed; this pins the switch this task makes, and it
    // pins the switch by content rather than by trusting the format string,
    // since a `RECORD_COLUMNS`-shaped read here would decode with
    // `FeedRecordRow` failing to find `xact_id_text` — a compile-time-invisible
    // mismatch a live read would still catch, but only behind Docker.
    let (sql, _binds) =
        build_feed_page_sql(None, None, &tenant_scope(Uuid::nil()), 10).expect("the scope renders");
    assert!(
        sql.contains(&format!(
            "SELECT {columns} FROM usage_records ",
            columns = crate::infra::storage::record_store::FEED_COLUMNS
        )),
        "the lateral body must select FEED_COLUMNS, not RECORD_COLUMNS: {sql}"
    );
}

#[test]
fn the_page_statement_is_a_lateral_join_over_the_subscription_not_an_any_array() {
    // The reshaping this task lands: `gts_type_id = ANY($1)` sorted the whole
    // remaining range before `LIMIT` (`tests/feed_page_query_plan_pg.rs`'s
    // finding, PostgreSQL derives no index pathkeys past a `ScalarArrayOpExpr`
    // on a leading index column). A lateral join over `unnest($1::text[])`
    // gives each subscribed type its own equality against
    // `usage_records_feed_idx`'s leading column, which keeps the index's
    // pathkeys and lets the inner `LIMIT` stop each type's walk early.
    let (sql, _binds) =
        build_feed_page_sql(None, None, &tenant_scope(Uuid::nil()), 10).expect("the scope renders");
    assert!(
        !sql.contains("gts_type_id = ANY($1)"),
        "the single ANY($1) equality must be gone: {sql}"
    );
    assert!(
        sql.contains("FROM unnest($1::text[]) AS t(gts) CROSS JOIN LATERAL"),
        "the subscription drives one lateral iteration per type: {sql}"
    );
    assert!(
        sql.contains("WHERE gts_type_id = t.gts"),
        "each iteration is an equality against the lateral row, not an array test: {sql}"
    );
}

#[test]
fn the_inner_and_outer_limits_match_and_the_outer_sort_casts_back_to_xid8() {
    // Ruling (spike candidate B, thing to get right #1): the inner LIMIT must
    // equal the outer one -- a smaller inner limit could silently drop a row
    // the outer sort needed, since a row in the global top `limit` has at
    // most `limit - 1` rows ahead of it within its own type. And the outer
    // ORDER BY must cast `xact_id_text` back to `xid8` rather than sort the
    // text, or two ids of different digit lengths misorder -- the same
    // digit-crossing hazard `CHUNK_HIGHEST_POSITIONS_SQL`'s own alias exists
    // to avoid.
    let (sql, _binds) =
        build_feed_page_sql(None, None, &tenant_scope(Uuid::nil()), 7).expect("the scope renders");
    assert_eq!(
        sql.matches("LIMIT 7").count(),
        2,
        "the same limit must be rendered inner and outer: {sql}"
    );
    assert!(
        sql.ends_with("ORDER BY s.xact_id_text::xid8, s.id LIMIT 7"),
        "the outer sort casts the text alias back to xid8 for numeric order: {sql}"
    );
}

#[test]
fn the_outer_projection_is_the_lateral_alias_star() {
    // The outer projection must preserve every alias the inner SELECT
    // produces (thing to get right #3) -- `s.*` does this by construction,
    // rather than by a second column list that could drift from
    // `FEED_COLUMNS` and silently stop matching `FeedRecordRow`'s by-name
    // decode.
    let (sql, _binds) =
        build_feed_page_sql(None, None, &tenant_scope(Uuid::nil()), 10).expect("the scope renders");
    assert!(
        sql.starts_with("SELECT s.* FROM unnest($1::text[])"),
        "the outer projection must be s.*, not a restated column list: {sql}"
    );
}

#[test]
fn a_continuation_bounds_position_from_below_as_one_row_value() {
    // Row-value comparison rather than the expanded disjunction, because it is
    // what `usage_records_feed_idx (gts_type_id, xact_id, id)` serves as a
    // single index condition **within one lateral iteration's equality on
    // `gts_type_id`** — the per-type read `query/feed.rs`'s own module doc
    // describes, not a single condition spanning the whole subscription.
    let (sql, _binds) = build_feed_page_sql(
        Some((41, Uuid::from_u128(7))),
        None,
        &tenant_scope(Uuid::nil()),
        3,
    )
    .expect("the scope renders");
    assert!(
        sql.contains("(xact_id, id) > (($3)::xid8, $4)"),
        "one row-value comparison, cast on the parameter: {sql}"
    );
}

#[test]
fn a_bounded_replay_bounds_position_from_above_too() {
    let (sql, _binds) = build_feed_page_sql(
        Some((41, Uuid::from_u128(7))),
        Some((99, Uuid::from_u128(8))),
        &tenant_scope(Uuid::nil()),
        3,
    )
    .expect("the scope renders");
    assert!(
        sql.contains("(xact_id, id) <= (($5)::xid8, $6)"),
        "inclusive on the upper bound, so a replay can reach its `until`: {sql}"
    );
}

#[test]
fn the_scope_binds_after_every_fixed_bind() {
    // The scope fragment's placeholders must start above the fixed binds, or
    // the values land on the wrong parameters. A first read has two fixed
    // binds, a continuation four, a bounded continuation six.
    for (after, until, first_scope_bind) in [
        (None, None, 3_usize),
        (Some((1, Uuid::nil())), None, 5),
        (Some((1, Uuid::nil())), Some((2, Uuid::nil())), 7),
    ] {
        let (sql, binds) = build_feed_page_sql(after, until, &tenant_scope(Uuid::from_u128(3)), 1)
            .expect("the scope renders");
        assert!(
            sql.contains(&format!("${first_scope_bind}")),
            "the scope's first placeholder is ${first_scope_bind}: {sql}"
        );
        assert_eq!(binds.len(), 1, "one scope bind for this scope");
    }
}

#[test]
fn an_unrenderable_scope_is_refused_before_any_connection_is_taken() {
    // `get` already translates the scope before acquiring a connection so a
    // scope that cannot be rendered never reaches the pool. The feed does the
    // same, and this is the assertion that keeps it true.
    let unrenderable = ast::Expr::Identifier("not_a_ledger_column".to_owned());
    assert!(
        build_feed_page_sql(None, None, &unrenderable, 1).is_err(),
        "an unrenderable scope is a builder error, not a query error"
    );
}

#[test]
fn the_mark_read_asks_only_whether_one_stands_above_the_position() {
    // It returns no row data at all — only whether a mark exists — which is
    // part of why this path has none of ruling C17's shape. And it is keyed on
    // the subscribed types alone: the mark is per GTS type and ignores the
    // compiled scope deliberately (DESIGN §3.6, Refusal granularity).
    assert!(
        MARK_ABOVE_SQL.contains("SELECT 1 FROM usage_feed_retention_marks"),
        "no row data: {MARK_ABOVE_SQL}"
    );
    assert!(
        MARK_ABOVE_SQL.contains("gts_type_id = ANY($1)"),
        "per subscribed type: {MARK_ABOVE_SQL}"
    );
    assert!(
        MARK_ABOVE_SQL.contains("(xact_id, id) > (($2)::xid8, $3)"),
        "strictly above the presented position: {MARK_ABOVE_SQL}"
    );
    assert!(
        !MARK_ABOVE_SQL.contains("tenant_id"),
        "the marks table carries no tenant column and the refusal is per type: \
         {MARK_ABOVE_SQL}"
    );
}
