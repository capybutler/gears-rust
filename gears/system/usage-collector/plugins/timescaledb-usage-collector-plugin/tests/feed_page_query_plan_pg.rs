#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `EXPLAIN`s the feed page's own statement over a live `TimescaleDB`, to
//! check the claim DESIGN §3.7, §4.1 item 7, `query/feed.rs`'s own module
//! doc, and `query/feed_tests.rs` all make: that the page is served by an
//! **index-ordered merge** over `usage_records_feed_idx (gts_type_id,
//! xact_id, id)`, with no sort in front of the `LIMIT`. Requires Docker.
//!
//! **Finding (ruling A3): the claim does not hold, and nothing changes it
//! here.** A `Sort` node sits above the index scan whether the subscription
//! names one type or several. This module's own job is to keep that finding
//! checkable, not to fix it — see the module doc below for why and the task
//! report for the plan text and reading handed to the owner.

mod common;

use rust_decimal::Decimal;
use sqlx::Row as _;
use toolkit_odata::ast;
use uuid::Uuid;

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::query::bind::bind_one_query;
use timescaledb_usage_collector_plugin::infra::storage::query::feed::build_feed_page_sql;

/// `EXPLAIN`s [`build_feed_page_sql`]'s statement over a two-chunk, two-type
/// subscription, forced onto `usage_records_feed_idx`, and reports whether
/// the plan needs a `Sort` in front of its `LIMIT`.
///
/// # Why the plan has to be forced onto `usage_records_feed_idx`
///
/// Left to its own cost model, `PostgreSQL` does not choose
/// `usage_records_feed_idx` at all at the row counts an integration test can
/// afford: with `gts_type_id = ANY($1)` bound as a parameter rather than a
/// literal, the planner has no way to know the array's selectivity at plan
/// time, so on a few hundred rows a sequential scan (or, with the other
/// ledger indexes still in place, `usage_records_tenant_type_window_idx` or
/// `usage_records_watermark_idx`, each a genuine alternative match for part
/// of the `WHERE`) costs less than an index scan every time. None of that
/// answers the question this test asks, which is specifically **whether
/// `usage_records_feed_idx` itself can produce `ORDER BY xact_id, id` for
/// free**. So the other three ledger indexes that could serve any part of
/// this statement's `WHERE` are dropped and `enable_seqscan` is turned off on
/// this test's own container, leaving `usage_records_feed_idx` the only
/// candidate — this is scan-shape isolation, not a claim about what a real
/// deployment's planner picks at its own scale, which is a **separate**,
/// unmeasured question (`docs/DESIGN.md` §4.1 item 7 already says so).
///
/// # The finding
///
/// **A `Sort` node sits above the index scan.** Verified twice here — once at
/// a two-type subscription (asserted below) and once, during this
/// investigation, at a one-type subscription (not landed as its own test,
/// since the two-type case already answers the question and a third
/// near-duplicate test buys nothing) — with the same result both times.
/// `PostgreSQL`'s planner does not derive index pathkeys past a
/// `ScalarArrayOpExpr` on a leading index column, regardless of how many
/// elements the bound array turns out to hold at execution time: pathkey
/// derivation is a property of the parsed expression shape
/// (`Var = ANY($1)`), decided once for the query's static structure, not of
/// the runtime cardinality a one-shot custom plan happens to see. A
/// subscription naming even a single type gets the same `Sort`.
///
/// # What this does and does not mean
///
/// `usage_records_feed_idx` is still what prunes the `Append` to the chunks
/// the type-key and time ranges the query names, and the `Sort` seen here
/// sees only that pruned, filtered set — not the whole hypertable. What it
/// costs is stated in `DESIGN.md` §4.1 item 7's own terms: the matching range
/// (bounded by the settled horizon on one side, `after` on the other) sorts
/// in full before `LIMIT` takes its slice, rather than the streaming,
/// already-ordered read the index-ordered-merge claim describes.
///
/// **This test does not decide whether that is a defect worth fixing.** The
/// fix would either restate four documents that currently claim otherwise
/// (`DESIGN.md` §3.7, `DESIGN.md` §4.1 item 7, `query/feed.rs`'s own module
/// doc, `query/feed_tests.rs`) or change the page statement's shape (for
/// instance, a per-type `MergeAppend` the application builds instead of
/// leaning on one `ScalarArrayOpExpr`) — a choice between those two is the
/// owner's, not this test's. This test's job is narrower: keep the finding
/// itself checkable, so a future change to the statement, the indexes, or
/// `PostgreSQL`'s own SAOP-pathkey support is noticed rather than silently
/// re-litigated. **If this assertion ever starts failing because the `Sort`
/// is gone, that is good news — update it to assert absence instead, and
/// `docs/features/usage-feed.md:672`'s acceptance line can be ticked.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_page_statement_still_sorts_after_usage_records_feed_idx() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let tenant = Uuid::from_u128(1);

    // Enough rows per type that the planner's cost model reflects a real
    // choice rather than tie-breaking among near-zero-cost plans, spread
    // across two types -- which, under the default `type_key_slice_width` of
    // 1, is also two chunks: each type gets its own type-key partition.
    for i in 0..200 {
        store
            .create(common::entry(
                &common::meter(common::VCPU_METER),
                tenant,
                &format!("plan-vcpu-{i}"),
                Decimal::ONE,
            ))
            .await
            .expect("vcpu entry stores");
        store
            .create(common::entry(
                &common::meter(common::GB_METER),
                tenant,
                &format!("plan-gb-{i}"),
                Decimal::ONE,
            ))
            .await
            .expect("gb entry stores");
    }
    sqlx::query("ANALYZE usage_records")
        .execute(&h.pool)
        .await
        .expect("analyze");
    let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM show_chunks('usage_records')")
        .fetch_one(&h.pool)
        .await
        .expect("count chunks");
    assert_eq!(chunks, 2, "one chunk per type at the default slice width");

    // A scope every fixture satisfies and that touches no other ledger index,
    // so the only index left standing that could serve any part of the
    // `WHERE` is `usage_records_feed_idx` itself.
    let scope = ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("resource_type".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String(
            "compute.vm".to_owned(),
        ))),
    );
    let (sql, binds) = build_feed_page_sql(None, None, &scope, 10).expect("the scope renders");
    let horizon: String =
        sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text")
            .fetch_one(&h.pool)
            .await
            .expect("read the settled horizon");

    // One connection for the whole block: a session-level `SET` issued
    // through the pool can land on a different pooled connection than the
    // statement that follows it, which would silently no-op it.
    let mut conn = h.pool.acquire().await.expect("acquire a connection");
    sqlx::query(
        "DROP INDEX usage_records_tenant_type_window_idx, usage_records_tenant_window_idx, \
         usage_records_watermark_idx",
    )
    .execute(&mut *conn)
    .await
    .expect("drop the other ledger indexes that could serve this WHERE");
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .expect("disable seqscan, so feed_idx is the only remaining candidate");

    let explain_sql = format!("EXPLAIN (FORMAT TEXT) {sql}");
    let types: Vec<&str> = vec![common::VCPU_METER, common::GB_METER];
    let mut q = sqlx::query(sqlx::AssertSqlSafe(explain_sql))
        .bind(types)
        .bind(&horizon);
    for b in &binds {
        q = bind_one_query(q, b);
    }
    let rows = q
        .fetch_all(&mut *conn)
        .await
        .expect("explain the page statement");
    let plan = rows
        .iter()
        .map(|row| row.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        plan.contains("usage_records_feed_idx"),
        "the isolation must have forced the scan onto the index this test is about; got:\n{plan}"
    );
    assert!(
        plan.contains("Sort"),
        "ruling A3: expected a Sort above the usage_records_feed_idx scan, matching the \
         finding reported to the owner -- if this now fails, the claim in DESIGN.md §3.7, \
         §4.1 item 7, query/feed.rs and query/feed_tests.rs has started holding, and this \
         assertion should invert rather than this test being deleted; full plan:\n{plan}"
    );
}
