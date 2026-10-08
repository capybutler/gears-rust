#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `EXPLAIN ANALYZE`s the feed page's own statement over a live
//! `TimescaleDB`, to check the property the statement's reshaping bought.
//! Requires Docker.
//!
//! **History.** This module originally checked the claim DESIGN §3.7, §4.1
//! item 7, `query/feed.rs`'s own module doc, and `query/feed_tests.rs` all
//! made: that the page was served by an index-ordered merge over
//! `usage_records_feed_idx (gts_type_uuid, xact_id, id)`, with no sort in front
//! of `LIMIT`. Ruling A3 found the claim did not hold — a `Sort` node sat
//! above the index scan regardless of subscription width, because
//! `PostgreSQL` derives no index pathkeys past a `ScalarArrayOpExpr` on a
//! leading index column. That finding was real, and it was acted on: two
//! reshapings were evaluated, and the owner chose the one `query/feed.rs`'s
//! own doc now carries — a lateral join over
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
//! **This is also why `docs/features/usage-feed.md:674` stays unticked.**
//! That acceptance line asserts a *merge* verified by the query plan. The
//! plan this module captures contains a real `Sort` node (a `top-N
//! heapsort` at width 8, `quicksort` at narrower widths), never a `Merge
//! Append`, so the literal claim does not hold under the chosen shape
//! (candidate B) — only the rejected alternative (candidate A, the per-type
//! `UNION ALL` this module's own history above did not choose) would have
//! produced one. The box is left open on purpose, not by oversight.

mod common;
use common::StoreFixtures;

use rust_decimal::Decimal;
use sqlx::Row as _;
use toolkit_odata::ast;
use usage_collector_sdk::{MeterTypeId, ResourceRef};
use uuid::Uuid;

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
                .create_fixture(common::entry(
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
        // `unnest($1::uuid[])`: the statement's subscription array is the
        // meters' registry references, which is what the ledger and
        // `usage_records_feed_idx` key on.
        let types: Vec<Uuid> = meters[..width]
            .iter()
            .map(|m| common::meter_ref(m.clone()).uuid)
            .collect();
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

/// The leading decimal number in `text`, if it starts with one.
fn leading_number(text: &str) -> Option<f64> {
    let digits: String = text
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse().ok()
}

/// Locates the sole analyzed scan over `usage_records_feed_idx` in `plan` and
/// returns its own line, the rows it returned, and how many index entries it
/// actually consumed: the rows returned **plus** the rows its `Filter`
/// discarded.
///
/// `EXPLAIN ANALYZE` reports an index scan's `rows=` *after* its filter, so
/// the returned count alone understates the read by exactly the discarded
/// rows — and the discarded rows are the whole subject here. Handing back the
/// scan line itself, rather than making the caller re-find it to check
/// `loops=`, is what keeps that check and these two figures reading the same
/// line: two independent searches for "the scan line" are two things that can
/// quietly drift apart.
///
/// Three parsing hazards this function exists to avoid, each of which would
/// return a plausible wrong number rather than fail:
///
/// * **More than one line can match.** This fixture is single-chunk, but
///   nothing here assumes that silently: a second chunk would put a second
///   analyzed scan over `usage_records_feed_idx` under the same
///   `ChunkAppend`, and taking the first match alone would report half the
///   consumption. This requires exactly one match and panics on zero or on
///   more than one.
/// * **The line carries `rows=` twice.** The first is inside
///   `(cost=… rows=… width=…)` and is the planner's *estimate*; only the one
///   after `actual time=` was measured. This splits on `actual time=` first,
///   as `sort_child_actual_rows` above does.
/// * **`Rows Removed by Filter` is absent entirely when the filter discarded
///   nothing.** `PostgreSQL`'s `show_instrumentation_count` emits that line in
///   text format only when the count is above zero, so its absence beside a
///   `Filter:` is unambiguous — it means zero, and it is the healthy
///   full-scope case rather than a parse failure. Treating it as a failure
///   would panic on the very comparison this test is built around.
///
/// # Panics
///
/// If `plan` carries zero, or more than one, analyzed scan line naming
/// `usage_records_feed_idx`, or if the matched line carries no `rows=` after
/// `actual time=`. The `actual time=` split itself cannot fail — the line was
/// selected because it already contains that substring — so its `expect`
/// below is defensive, not a documented reachable outcome.
fn feed_idx_scan_figures(plan: &str) -> (&str, f64, f64) {
    let lines: Vec<&str> = plan.lines().collect();
    let matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.contains("usage_records_feed_idx") && line.contains("actual time=")
        })
        .map(|(idx, _)| idx)
        .collect();
    let scan_idx = match matches.as_slice() {
        [] => panic!("no analyzed scan over usage_records_feed_idx in plan:\n{plan}"),
        [idx] => *idx,
        _ => panic!(
            "{} analyzed scans over usage_records_feed_idx in one plan -- this oracle sums a \
             single scan's own line, and a second one would silently halve the reported \
             consumption; plan:\n{plan}",
            matches.len()
        ),
    };
    let scan_line = lines[scan_idx];
    let after_actual = scan_line
        .split_once("actual time=")
        .expect("the line was selected because it already contains \"actual time=\"");
    let returned = after_actual
        .1
        .split_once("rows=")
        .and_then(|(_, tail)| leading_number(tail))
        .unwrap_or_else(|| panic!("the scan carries no actual rows=:\n{plan}"));

    // The scan's own detail lines run until the next node arrow. An absent
    // `Rows Removed by Filter` means zero -- see the note above.
    let removed = lines[scan_idx + 1..]
        .iter()
        .copied()
        .take_while(|line| !line.contains("->"))
        .find_map(|line| {
            line.split_once("Rows Removed by Filter: ")
                .and_then(|(_, tail)| leading_number(tail))
        })
        .unwrap_or(0.0);
    (scan_line, returned, returned + removed)
}

/// `EXPLAIN ANALYZE`s the feed page's own statement at a **fixed** subscription
/// width while varying only the compiled scope's selectivity, and asserts a
/// narrow scope consumes at least as much of `usage_records_feed_idx` as a full
/// one does to fill the same page.
///
/// # What this measures
///
/// `cpt-cf-uc-plugin-dod-replay-read-rate` states that a consumer whose scope
/// admits a small share of a subscription still reads the whole subscription's
/// index range to fill a page. `usage_records_feed_idx` is
/// `(gts_type_uuid, xact_id, id)` and the scope names none of those columns, so
/// [`build_feed_page_sql`] can only render the scope as a `Filter` above the
/// scan. Narrowing the scope therefore cannot narrow the index range read; it
/// can only force the scan further along it.
///
/// # What this does NOT measure, and why the box stays unticked
///
/// **No replay rate is produced here.** This runs under no load, drives no
/// envelope, and times nothing. `docs/DESIGN.md` §4.1 item 7 names two
/// different tests in consecutive sentences, and they are not interchangeable.
/// The *confirming test* is the one its first sentence describes: a
/// measurement of a narrow scope as well as a full one, which is what this
/// module performs — see the two paragraphs below for what that does and does
/// not establish. The test it marks **Not measured** is the one that would
/// produce the replay-rate **number**: a 24-hour backlog replay of a
/// subscription at the launch arrival rate while ingestion runs at the
/// throughput-profile envelope, also timing a first read's first page. No such
/// replay test exists in this repository, this module is not one, and that
/// number stays unmeasured, as `README.md` already records for every figure of
/// this class.
///
/// **A residue survives on the index-range side too, and it is this module's
/// own.** The figures below are taken under forced scan-shape isolation — four
/// indexes removed from contention, `enable_seqscan = off` — at subscription
/// width 1, one chunk, forty rows. Item 7's own first half speaks of the index
/// range read *across the chunks retention keeps*, and a single-chunk
/// forced-plan measurement does not establish that. The feature's own
/// acceptance line for this property (`docs/features/usage-feed.md:674`) asks
/// for the same thing in its own words — verification *across more than one
/// chunk* — and this fixture is single-chunk, so that line is cited here, not
/// satisfied.
///
/// **Nothing here is ticked, and no verdict on the clause is claimed.** The
/// open clause (`docs/features/usage-feed.md:620-621`) reads *"the confirming
/// test **MUST** measure a narrow scope as well as a full one"*. This module
/// measures both. Whether that satisfies the clause is a tick decision
/// reserved to the repository owner, who has not made it, so the box stays
/// open: this slice does not claim the clause closed, and the call is the
/// owner's on the clause's own wording.
///
/// # Why subscription width is fixed at 1
///
/// `EXPLAIN ANALYZE` reports a node's `rows=` as a **per-loop average** when
/// the node runs more than once. A wider subscription makes the lateral join
/// iterate, and every figure below would silently become a mean. Width 1 keeps
/// `loops=1`, and the test asserts that rather than assuming it. The bound that
/// scales with width is the neighbouring test's subject, not this one's.
///
/// # Why the index isolation is repeated rather than shared, and completed by a fourth mechanism
///
/// Same reason the neighbouring test gives at its own site: left to its own
/// cost model `PostgreSQL` does not choose `usage_records_feed_idx` at the row
/// counts an integration test can afford. The other three ledger indexes are
/// dropped and `enable_seqscan` is turned off on this test's own container,
/// same as the neighbour. This test needs a fourth mechanism the neighbour
/// does not: the `usage_records_dedup_uniq` UNIQUE constraint is also
/// dropped, because its backing btree can serve part of this `WHERE` once the
/// scope is selective enough — the inline comment at that `DROP CONSTRAINT`
/// site carries the full reasoning and what still survives all four drops.
/// This is scan-shape isolation, not a claim about a real deployment's
/// planner, and none of it claims `usage_records_feed_idx` is the only index
/// structurally capable of surviving — see that same site for what else is
/// still standing and why it is not competitive for this statement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_narrow_scope_consumes_at_least_as_much_of_the_feed_index_as_a_full_one() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let tenant = Uuid::from_u128(2);
    let limit: u64 = 10;
    let rows_per_type: u64 = 40;

    // One type, forty entries, four of which carry the narrow scope's resource
    // id. Four is below `limit`, so a page under the narrow scope cannot stop
    // early: the scan must walk the type's whole retained range and still come
    // back short. Nothing here varies `id` derivation -- `resource_ref` is not
    // one of the six identity inputs, so overwriting it after construction
    // leaves each entry's derived identifier untouched.
    let meter = common::meter("gts.cf.core.uc.usage_record.v1~cf.meter0._.units0.v1~");
    for i in 0..rows_per_type {
        let mut e = common::entry(&meter, tenant, &format!("scope-plan-{i}"), Decimal::ONE);
        let resource_id = if i % 10 == 0 {
            "res-rare"
        } else {
            "res-common"
        };
        e.resource_ref = ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref");
        store.create_fixture(e).await.expect("entry stores");
    }
    sqlx::query("ANALYZE usage_records")
        .execute(&h.pool)
        .await
        .expect("analyze");

    // Full: every fixture row satisfies it. Narrow: four of forty do.
    let full = ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("resource_type".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String(
            "compute.vm".to_owned(),
        ))),
    );
    let narrow = ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("resource_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String("res-rare".to_owned()))),
    );

    let horizon: String =
        sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text")
            .fetch_one(&h.pool)
            .await
            .expect("read the settled horizon");

    // One connection for the whole block: a session-level `SET` issued through
    // the pool can land on a different pooled connection than the statement
    // that follows it, which would silently no-op it.
    let mut conn = h.pool.acquire().await.expect("acquire a connection");
    sqlx::query(
        "DROP INDEX usage_records_tenant_type_window_idx, usage_records_tenant_window_idx, \
         usage_records_watermark_idx",
    )
    .execute(&mut *conn)
    .await
    .expect("drop the other ledger indexes that could serve this WHERE");
    // A fourth access path survives the three drops above: the btree backing
    // `usage_records_dedup_uniq` -- UNIQUE (tenant_id, gts_type_uuid, ...)
    // (migrations/0001_init.sql:151-152) -- leads on `tenant_id`, but this
    // fixture writes every row under one tenant, so `tenant_id` has exactly
    // one distinct value in the chunk and PostgreSQL 18's btree skip scan can
    // reach `gts_type_uuid` through it almost for free. A narrow enough scope
    // then makes a bitmap scan over that index look cheaper than
    // `usage_records_feed_idx`, which defeats the isolation below silently
    // rather than loudly. This constraint exists to enforce dedup identity,
    // not to serve feed reads, and this container is per-test and throwaway:
    // every one of the 40 rows this test writes carries a distinct
    // idempotency key, so nothing this test measures depends on the
    // constraint being enforced. Dropping it is completing the isolation's
    // own stated intent, not a planner-setting search for a passing number.
    sqlx::query("ALTER TABLE usage_records DROP CONSTRAINT usage_records_dedup_uniq")
        .execute(&mut *conn)
        .await
        .expect(
            "drop the dedup UNIQUE constraint, whose backing index is the fourth access path \
             that could serve this WHERE and that the three DROP INDEX statements above cannot \
             reach",
        );
    // What survives all four drops. Written in migrations/0001_init.sql:
    // PRIMARY KEY (id, window_end, type_key), and the partial index
    // usage_records_invalidates_idx (invalidates, window_end, type_key) WHERE
    // invalidates IS NOT NULL. Not written there but built anyway:
    // TimescaleDB's own default indexes on the partitioning columns --
    // migrations/0001_init.sql:185-186 calls create_hypertable(...
    // by_range('window_end')) and add_dimension(... by_range('type_key', 1))
    // with create_default_indexes left at its TRUE default, which nothing in
    // this tree overrides, so an index on window_end and one on type_key exist
    // without appearing in any CREATE INDEX statement. None of these is a
    // candidate for this statement. The partial index cannot be used at all
    // unless this query's WHERE implies `invalidates IS NOT NULL`, which it
    // does not -- every fixture row here has `invalidates = NULL` -- and
    // PostgreSQL excludes an unimplied partial index categorically, not on
    // cost. The columns the rest of them lead on (id, window_end, type_key)
    // appear nowhere in this statement's WHERE, which carries gts_type_uuid,
    // xact_id and the compiled scope and nothing else
    // (src/infra/storage/query/feed.rs:154-176), so no condition here can
    // narrow a scan over any of them; a full scan of any one would still read
    // every row in the chunk, which costs strictly more than
    // usage_records_feed_idx's selective Index Cond on gts_type_uuid. So
    // usage_records_feed_idx is the cheapest remaining candidate for this
    // statement, not the only one structurally possible -- the SET below is
    // what makes the planner's cost model prefer it over a sequential scan,
    // not what removes other index candidates.
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .expect(
            "disable seqscan: at this fixture's forty rows per chunk, a sequential scan costs \
             less than every index path, so without this the planner never takes the index \
             path this test is about",
        );

    let types = vec![common::meter_ref(meter.clone()).uuid];
    let mut consumed: Vec<(&str, f64, f64)> = Vec::new();
    for (label, scope) in [("full", &full), ("narrow", &narrow)] {
        let (sql, binds) = build_feed_page_sql(None, None, scope, limit)
            .unwrap_or_else(|e| panic!("the {label} scope renders: {e}"));
        let explain_sql = format!("EXPLAIN (ANALYZE, FORMAT TEXT) {sql}");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(explain_sql))
            .bind(types.clone())
            .bind(&horizon);
        for b in &binds {
            q = bind_one_query(q, b);
        }
        let plan = q
            .fetch_all(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("explain analyze the page statement, {label} scope: {e}"))
            .iter()
            .map(|row| row.get::<String, _>(0))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            plan.contains("usage_records_feed_idx"),
            "{label} scope: the isolation must have forced the scan onto the index this \
             test is about. A scan that moved elsewhere means some other path became \
             cheaper for this statement than usage_records_feed_idx -- a surviving index, \
             or a sequential scan, which enable_seqscan = off penalizes rather than \
             removes -- so read the DROP and SET block above before trusting any number \
             below; plan:\n{plan}"
        );

        let (scan_line, returned, scan_consumed) = feed_idx_scan_figures(&plan);

        // Asserted on the scan's OWN line, never on the whole plan text: the
        // top-level Limit node carries `loops=1` unconditionally, so a
        // `plan.contains("loops=1")` would pass whatever the scan did and
        // assert nothing at all. The trailing `)` is load-bearing too: a bare
        // `"loops=1"` is also a substring of `loops=10`, `loops=12` and
        // `loops=100`, none of which are the `loops=1` this test requires.
        assert!(
            scan_line.contains("loops=1)"),
            "{label} scope: this test reads raw row counts, which EXPLAIN reports as \
             per-loop averages once a node runs more than once. It fixes the \
             subscription at one type so the scan's own loops stays 1; scan line was \
             `{scan_line}` in plan:\n{plan}"
        );

        consumed.push((label, scan_consumed, returned));
    }

    let (_, full_consumed, full_returned) = consumed[0];
    let (_, narrow_consumed, narrow_returned) = consumed[1];

    // `limit` and `rows_per_type` are 10 and 40 -- nowhere near f64's 52-bit
    // mantissa, so the casts lose nothing.
    #[allow(clippy::cast_precision_loss)]
    let limit_f64 = limit as f64;
    #[allow(clippy::cast_precision_loss)]
    let whole_range = rows_per_type as f64;

    // Guard against the headline assertion passing for the wrong reason: a
    // narrow scope matching nothing also reads the whole range, and would
    // prove nothing about scope at all.
    assert!(
        narrow_returned > 0.0 && narrow_returned < limit_f64,
        "the narrow scope must match some rows but fewer than one page, or this test \
         passes without exercising scope selectivity at all; it returned \
         {narrow_returned}"
    );
    assert!(
        (full_returned - limit_f64).abs() < f64::EPSILON,
        "the full scope must fill a whole page, or the two scopes are not being \
         compared at the same page size; it returned {full_returned}"
    );

    assert!(
        narrow_consumed >= full_consumed,
        "narrowing the scope must not buy a cheaper read: the full scope consumed \
         {full_consumed} index entries and the narrow one consumed {narrow_consumed}. \
         A narrow scope reading less means the scope reached the index as a condition \
         rather than staying a filter above it, which is what this claim denies."
    );
    assert!(
        narrow_consumed >= whole_range,
        "a scope admitting fewer rows than one page must walk the whole retained range \
         for its type: {rows_per_type} entries were written and the scan consumed \
         {narrow_consumed}"
    );
}
