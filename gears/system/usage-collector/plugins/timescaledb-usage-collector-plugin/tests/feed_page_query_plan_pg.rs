#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `EXPLAIN ANALYZE`s the feed page's own statement over a live
//! `TimescaleDB`, to check the property the statement's reshaping bought.
//! Requires Docker.
//!
//! **History.** This module originally checked the claim DESIGN §3.7, §4.1
//! item 7, `query/feed.rs`'s own module doc, and `query/feed_tests.rs` all
//! made: that the page was served by an index-ordered merge over
//! `usage_records_feed_idx (gts_type_id, xact_id, id)`, with no sort in front
//! of `LIMIT`. Ruling A3 found the claim did not hold — a `Sort` node sat
//! above the index scan regardless of subscription width, because
//! `PostgreSQL` derives no index pathkeys past a `ScalarArrayOpExpr` on a
//! leading index column. That finding was real, and it was acted on: a spike
//! (`.superpowers/sdd/2026-09-27-usage-collector-spec-complete/
//! spike-feed-page-index-order.md`) evaluated two reshapings, and the owner
//! chose the one `query/feed.rs`'s own doc now carries — a lateral join over
//! `unnest($1::text[])`, one equality-driven, pathkey-preserving iteration
//! per subscribed type, combined by an outer sort.
//!
//! **Why this module's old assertion is gone, not inverted.** Under the
//! chosen reshaping **a `Sort` node still exists** — it sits above the
//! lateral join rather than above a single scan. Asserting `plan.contains("Sort")`
//! would still pass, but it would no longer be testing anything: a `Sort`
//! over the whole backlog and a `Sort` over `width × limit` rows both contain
//! the word "Sort". The property the reshaping actually bought — and the one
//! the spike measured — is that the sort's own input is *bounded*, not that
//! a sort node is present or absent. This module now asserts that bound
//! directly, at more than one subscription width, so the bound is shown to
//! scale with breadth rather than being a coincidence at one width.
//!
//! **This is also why `docs/features/usage-feed.md:672` stays unticked.**
//! That acceptance line asserts a *merge* verified by the query plan. The
//! plan this module captures contains a real `Sort` node (a `top-N
//! heapsort` at width 8, `quicksort` at narrower widths), never a `Merge
//! Append`, so the literal claim does not hold under the chosen shape
//! (candidate B) — only the rejected alternative (candidate A, the per-type
//! `UNION ALL` this module's own history above did not choose) would have
//! produced one. The box is left open on purpose, not by oversight.

mod common;

use rust_decimal::Decimal;
use sqlx::Row as _;
use toolkit_odata::ast;
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::query::bind::bind_one_query;
use timescaledb_usage_collector_plugin::infra::storage::query::feed::build_feed_page_sql;

/// The actual row count `EXPLAIN ANALYZE` reports flowing **into** the page
/// statement's outer `Sort` node — its immediate child's own `actual rows=`,
/// read off the plan text rather than the `Sort` node's own cost estimate or
/// its (`LIMIT`-shortened) output count, since the property under test is
/// what the sort had to process, not what it was asked to return or what the
/// planner guessed beforehand.
///
/// # Panics
///
/// If no `Sort` node, or no child line beneath it carrying `actual time=` and
/// `rows=`, can be found in `plan`. A parse failure here must be loud: a
/// silently wrong number would make this oracle worthless.
fn sort_child_actual_rows(plan: &str) -> f64 {
    let lines: Vec<&str> = plan.lines().collect();
    let sort_idx = lines
        .iter()
        .position(|line| {
            let trimmed = line.trim_start_matches(['-', '>', ' ']);
            trimmed.starts_with("Sort") && line.contains("(cost=")
        })
        .unwrap_or_else(|| panic!("no Sort node found in plan:\n{plan}"));
    let child = lines[sort_idx + 1..]
        .iter()
        .find(|line| line.contains("->"))
        .unwrap_or_else(|| panic!("no child node found beneath Sort in plan:\n{plan}"));
    let after_actual = child.split_once("actual time=").unwrap_or_else(|| {
        panic!("Sort's child carries no actual time=, so this plan was not ANALYZEd:\n{plan}")
    });
    let after_rows = after_actual
        .1
        .split_once("rows=")
        .unwrap_or_else(|| panic!("Sort's child carries no actual rows=:\n{plan}"));
    let digits: String = after_rows
        .1
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits
        .parse()
        .unwrap_or_else(|e| panic!("could not parse actual rows `{digits}`: {e}\n{plan}"))
}

/// `EXPLAIN ANALYZE`s [`build_feed_page_sql`]'s statement, forced onto
/// `usage_records_feed_idx`, at subscription widths 1, 2 and 8 — the same
/// widths the spike measured — and asserts the outer `Sort`'s own input never
/// exceeds `width × limit` actual rows.
///
/// # Why the plan has to be forced onto `usage_records_feed_idx`
///
/// Left to its own cost model, `PostgreSQL` does not choose
/// `usage_records_feed_idx` at all at the row counts an integration test can
/// afford: with the subscribed types bound as parameters rather than
/// literals, the planner has no way to know their selectivity at plan time,
/// so on a few hundred rows a sequential scan (or, with the other ledger
/// indexes still in place, `usage_records_tenant_type_window_idx` or
/// `usage_records_watermark_idx`, each a genuine alternative match for part
/// of the `WHERE`) costs less than an index scan every time. So the other
/// three ledger indexes that could serve any part of this statement's
/// `WHERE` are dropped and `enable_seqscan` is turned off on this test's own
/// container, leaving `usage_records_feed_idx` the only candidate — this is
/// scan-shape isolation, not a claim about what a real deployment's planner
/// picks at its own scale, which is a **separate**, unmeasured question
/// (`docs/DESIGN.md` §4.1 item 7 already says so).
///
/// # The bound
///
/// **The correctness argument requires the inner, per-type `LIMIT` to equal
/// the outer one** — a row in the global top `limit` has at most `limit − 1`
/// rows ahead of it within its own type, so per-type top-`limit` then combine
/// yields exactly the global top `limit`; a smaller inner limit could
/// silently drop a row. That equality is also what keeps the outer sort
/// bounded: each of `width` lateral iterations contributes at most `limit`
/// rows, so the sort above them never sees more than `width × limit`,
/// whatever the subscription's total backlog. This test's fixture (eight
/// types, forty rows each, one chunk per type at the default slice width)
/// makes the backlog per type large enough that an unbounded sort — the
/// defect ruling A3 found — would visibly exceed the bound at every width
/// tested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sort_above_usage_records_feed_idx_stays_bounded_by_width_times_limit() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let tenant = Uuid::from_u128(1);
    let limit: u64 = 10;

    // Eight distinct types, forty rows each -- one chunk per type at the
    // default `type_key_slice_width` of 1, matching the spike's own fixture.
    // Built once; each width below just names a narrower prefix of it.
    let meters: Vec<MeterTypeId> = (0..8u32)
        .map(|i| {
            common::meter(&format!(
                "gts.cf.core.uc.usage_record.v1~cf.meter{i}._.units{i}.v1~"
            ))
        })
        .collect();
    for m in &meters {
        for i in 0..40 {
            store
                .create(common::entry(
                    m,
                    tenant,
                    &format!("{}-plan-{i}", m.as_str()),
                    Decimal::ONE,
                ))
                .await
                .expect("entry stores");
        }
    }
    sqlx::query("ANALYZE usage_records")
        .execute(&h.pool)
        .await
        .expect("analyze");
    let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM show_chunks('usage_records')")
        .fetch_one(&h.pool)
        .await
        .expect("count chunks");
    assert_eq!(chunks, 8, "one chunk per type at the default slice width");

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

    // Built once: `sql` and `binds` do not vary with `width`, only which
    // prefix of `meters` is bound into `$1` below does.
    let (sql, binds) = build_feed_page_sql(None, None, &scope, limit).expect("the scope renders");
    let explain_sql = format!("EXPLAIN (ANALYZE, FORMAT TEXT) {sql}");

    for width in [1usize, 2, 8] {
        let types: Vec<&str> = meters[..width].iter().map(MeterTypeId::as_str).collect();
        let mut q = sqlx::query(sqlx::AssertSqlSafe(explain_sql.clone()))
            .bind(types)
            .bind(&horizon);
        for b in &binds {
            q = bind_one_query(q, b);
        }
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("explain analyze the page statement at width {width}: {e}"));
        let plan = rows
            .iter()
            .map(|row| row.get::<String, _>(0))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            plan.contains("usage_records_feed_idx"),
            "width {width}: the isolation must have forced the scan onto the index this test \
             is about; got:\n{plan}"
        );

        let bound = width as u64 * limit;
        let actual = sort_child_actual_rows(&plan);
        // `bound` is `width x limit`, at most 8 x 10 = 80 here -- nowhere near
        // f64's 52-bit mantissa, so the cast loses nothing.
        #[allow(clippy::cast_precision_loss)]
        let bound_f64 = bound as f64;
        assert!(
            actual <= bound_f64,
            "width {width}: the sort above usage_records_feed_idx was fed {actual} actual \
             rows, which must stay at or below width x limit = {bound} -- if this fails, the \
             lateral join's per-type LIMIT has stopped pushing down, or the inner and outer \
             LIMITs have drifted apart; plan:\n{plan}"
        );
    }
}
