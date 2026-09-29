#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `StorageAdapter::read_feed_page` against a live `TimescaleDB`. Requires
//! Docker.
//!
//! This is the plugin's own behavioural coverage of the page protocol
//! (`docs/DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-feed-page`), alongside the
//! DESIGN section 3.3 contract suite's more abstract obligations
//! (`tests/contract_conformance_pg.rs`). Every read goes through
//! [`StorageAdapter::read_feed_page`] rather than `PgRecordStore::feed_page`
//! directly, so the position codec (`src/infra/storage/feed_position.rs`) and
//! the limit / start-mode validation the adapter owns are exercised along
//! with the store.
//!
//! Each test starts its own container ([`common::start_backend`]), so no test
//! here shares a ledger with another and tenants and idempotency keys are
//! reused freely across them.

mod common;

use rust_decimal::Decimal;
use toolkit_odata::ast;
use uuid::Uuid;

use usage_collector_sdk::{
    FeedPosition, FeedStart, UsageCollectorPluginError, UsageCollectorPluginV1,
};

use timescaledb_usage_collector_plugin::infra::storage::feed_position::encode_position;
use timescaledb_usage_collector_plugin::infra::storage::query::MAX_PAGE_SIZE;
use timescaledb_usage_collector_plugin::infra::storage::retention_sweep::RAISE_MARKS_SQL;

/// Raise a retention mark directly, via the sweep's own [`RAISE_MARKS_SQL`]
/// statement — the same "raise, never lower" write the production sweep
/// issues, bound over exactly one type.
///
/// Every caller of this helper is testing the page's *read* of a mark, not
/// the sweep's *write* of one (ruling D7): the sweep's write is pinned by
/// `retention_sweep_tests::the_mark_raise_only_ever_raises`, and what joins
/// the write to a real chunk drop is Task 5's driven dispatch. Calling this
/// directly is what lets a page-side test land before that drive exists,
/// without duplicating the sweep's own SQL.
async fn raise_mark(pool: &sqlx::PgPool, gts_type_id: &str, xact_id: u64, id: Uuid) {
    sqlx::query(RAISE_MARKS_SQL)
        .bind(vec![gts_type_id.to_owned()])
        .bind(vec![xact_id.to_string()])
        .bind(vec![id])
        .execute(pool)
        .await
        .expect("raise a retention mark directly");
}

// 1. A first read begins at the oldest entry and carries a continuation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_read_begins_at_the_oldest_entry_and_carries_a_continuation() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let first = common::entry(&meter, tenant, "feed-first", Decimal::ONE);
    let second = common::entry(&meter, tenant, "feed-second", Decimal::ONE);
    adapter
        .create_usage_record(first.clone())
        .await
        .expect("the first entry is accepted");
    adapter
        .create_usage_record(second.clone())
        .await
        .expect("the second entry is accepted");

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is served");

    assert_eq!(
        page.entries.first().map(|entry| entry.id),
        Some(first.id),
        "a first read begins at the oldest entry the subscription retains: {:?}",
        page.entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
    );
    assert!(
        page.next.is_some(),
        "a live read carries a continuation on every page"
    );
}

// 2. A first read over a meter nothing was written to is served, carries no
//    entry, and carries a continuation. <-- Review Focus 3
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_read_over_an_empty_meter_is_served_and_carries_a_head_cursor() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter];

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a subscription retaining nothing is served, never refused");

    assert!(
        page.entries.is_empty(),
        "nothing has ever been written to this meter: {:?}",
        page.entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
    );
    assert!(
        page.next.is_some(),
        "an empty page still carries a head cursor, never `None`"
    );
}

// 3. A subscription naming an empty meter beside a busy one delivers exactly
//    what the busy meter alone delivers. <-- Review Focus 3
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_meter_named_beside_a_busy_one_changes_nothing() {
    let (_h, adapter) = common::start_backend().await;
    let busy = common::meter(common::VCPU_METER);
    let empty = common::meter(common::GB_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);

    let entry = common::entry(&busy, tenant, "feed-busy", Decimal::ONE);
    adapter
        .create_usage_record(entry.clone())
        .await
        .expect("the entry is accepted");

    let alone = adapter
        .read_feed_page(
            std::slice::from_ref(&busy),
            &scope,
            FeedStart::Oldest,
            None,
            10,
        )
        .await
        .expect("the busy meter alone is served");
    let beside = adapter
        .read_feed_page(&[busy, empty], &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("the busy meter beside an empty one is served");

    assert_eq!(
        alone.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        beside.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        "an empty meter named beside a busy one contributes nothing of its own"
    );
}

// 4. Two entries of one batch share an `xact_id` and are ordered by `id`.
//
// **Not built on `create_usage_records`.** `run_guarded_batch_write`'s own
// doc requires its representatives sorted by `entry_identity` before the
// write, for deadlock avoidance — so a batch's own rows are *always*
// physically inserted in id order regardless of the caller's order, and a
// tie built through it would match the `ORDER BY xact_id, id` tiebreak by
// coincidence even with the tiebreak deleted. Measured, not assumed: an
// earlier version of this test built its tie through `create_usage_records`
// and stayed green under `ORDER BY xact_id, id` mutated to `ORDER BY
// xact_id` alone — the mutation this test exists to catch. Two rows inserted
// directly, in one transaction, in the *reverse* of their id order, is what
// makes physical order and id order disagree, so only the tiebreak itself
// can produce the expected result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_entries_sharing_a_transaction_id_are_ordered_by_id_not_insertion_order() {
    let (h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let e1 = common::entry(&meter, tenant, "feed-batch-1", Decimal::ONE);
    let e2 = common::entry(&meter, tenant, "feed-batch-2", Decimal::ONE);
    let mut expected = [e1.id, e2.id];
    expected.sort_unstable();
    // Insert the *greater* id first and the *lesser* second: the reverse of
    // the order the tiebreak must produce.
    let (first, second) = if e1.id == expected[1] {
        (&e1, &e2)
    } else {
        (&e2, &e1)
    };

    let mut tx = h.pool.begin().await.expect("begin the shared transaction");
    insert_raw_within(&mut tx, first).await;
    insert_raw_within(&mut tx, second).await;
    tx.commit()
        .await
        .expect("commit both rows under one transaction id");

    let x1 = common::xact_id_of(&h.pool, e1.id).await;
    let x2 = common::xact_id_of(&h.pool, e2.id).await;
    assert_eq!(x1, x2, "both rows were committed under one transaction id");

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is served");

    assert_eq!(
        page.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        expected.to_vec(),
        "two entries sharing an xact_id are ordered by id, the feed order's tiebreak, \
         even though they were inserted in the opposite order"
    );
}

// 5. A page filled to its limit continues from the last entry's position, and
//    following it delivers the rest exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_filled_to_its_limit_continues_and_the_rest_arrives_exactly_once() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let mut ids = Vec::new();
    for i in 0..5 {
        let entry = common::entry(&meter, tenant, &format!("feed-limit-{i}"), Decimal::ONE);
        adapter
            .create_usage_record(entry.clone())
            .await
            .expect("the entry is accepted");
        ids.push(entry.id);
    }

    let first_page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 2)
        .await
        .expect("a first read is served");
    assert_eq!(
        first_page.entries.len(),
        2,
        "a page filled to its limit carries exactly that many entries"
    );

    let mut delivered: Vec<Uuid> = first_page.entries.iter().map(|r| r.id).collect();
    let mut start = FeedStart::After(
        first_page
            .next
            .expect("a filled page still carries a continuation"),
    );
    for _ in 0..10 {
        if delivered.len() >= ids.len() {
            break;
        }
        let page = adapter
            .read_feed_page(&subscription, &scope, start.clone(), None, 2)
            .await
            .expect("a continuation is served");
        delivered.extend(page.entries.iter().map(|r| r.id));
        start = FeedStart::After(page.next.expect("a live page carries a continuation"));
    }

    assert_eq!(
        delivered, ids,
        "every entry arrives exactly once, in feed order, across as many pages as it takes"
    );
}

// 6. A bounded replay reaching its `until` carries no continuation, and the
//    same replay run twice delivers the same entries in the same order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bounded_replay_closes_at_its_until_and_repeats_identically() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let e1 = common::entry(&meter, tenant, "feed-bounded-1", Decimal::ONE);
    let e2 = common::entry(&meter, tenant, "feed-bounded-2", Decimal::ONE);
    adapter
        .create_usage_record(e1.clone())
        .await
        .expect("the first entry is accepted");
    adapter
        .create_usage_record(e2.clone())
        .await
        .expect("the second entry is accepted");

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is served");
    assert_eq!(page.entries.len(), 2, "both entries settle on one page");
    let until = page.next.expect("a live page carries a continuation");

    for attempt in 0..2 {
        let replay = adapter
            .read_feed_page(
                &subscription,
                &scope,
                FeedStart::Oldest,
                Some(until.clone()),
                10,
            )
            .await
            .expect("a bounded replay is served");
        assert_eq!(
            replay.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![e1.id, e2.id],
            "attempt {attempt}: a bounded replay delivers the same entries in the same order"
        );
        assert!(
            replay.next.is_none(),
            "attempt {attempt}: a bounded replay that reached its `until` carries no continuation"
        );
    }
}

// 7. A limit of 0 and a limit of MAX_PAGE_SIZE + 1 are each refused as
//    `Internal` naming the bound. <-- Review Focus 1
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limit_outside_the_published_bound_is_refused_naming_it() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter];

    for limit in [0, MAX_PAGE_SIZE + 1] {
        let err = adapter
            .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, limit)
            .await
            .expect_err(&format!(
                "a limit of {limit} is outside the published bound"
            ));
        assert!(
            matches!(err, UsageCollectorPluginError::Internal(_)),
            "refused as Internal: {err}"
        );
        let detail = err.to_string();
        assert!(
            detail.contains(&MAX_PAGE_SIZE.to_string()),
            "the detail names the published bound {MAX_PAGE_SIZE}: {detail}"
        );
    }
}

// 8. A position of the wrong width, and a `FeedStart::After` carrying one, are
//    each refused as `Internal`. <-- Review Focus 2
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_position_of_the_wrong_width_is_refused_as_internal() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter];

    let wrong_width = FeedPosition::new(vec![0_u8; 10]).expect("a well-formed, in-bound position");

    let err = adapter
        .read_feed_page(
            &subscription,
            &scope,
            FeedStart::After(wrong_width.clone()),
            None,
            10,
        )
        .await
        .expect_err("a `FeedStart::After` position of the wrong width is refused");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "refused as Internal: {err}"
    );

    let err = adapter
        .read_feed_page(
            &subscription,
            &scope,
            FeedStart::Oldest,
            Some(wrong_width),
            10,
        )
        .await
        .expect_err("an `until` position of the wrong width is refused too");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "refused as Internal: {err}"
    );
}

// 9. The scope narrows: an entry of another tenant is absent under a scope
//    admitting only the first, and present under one admitting both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scope_narrows_the_feed_to_the_tenants_it_admits() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant_a = Uuid::from_u128(1);
    let tenant_b = Uuid::from_u128(2);
    let subscription = [meter.clone()];

    let a = common::entry(&meter, tenant_a, "feed-scope-a", Decimal::ONE);
    let b = common::entry(&meter, tenant_b, "feed-scope-b", Decimal::ONE);
    adapter
        .create_usage_record(a.clone())
        .await
        .expect("tenant a's entry is accepted");
    adapter
        .create_usage_record(b.clone())
        .await
        .expect("tenant b's entry is accepted");

    let narrow_scope = common::tenant_scope(tenant_a);
    let narrow = adapter
        .read_feed_page(&subscription, &narrow_scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read under the narrow scope is served");
    assert_eq!(
        narrow.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![a.id],
        "the second tenant's entry is absent under a scope admitting only the first"
    );

    let wide_scope = ast::Expr::Or(
        Box::new(common::tenant_scope(tenant_a)),
        Box::new(common::tenant_scope(tenant_b)),
    );
    let wide = adapter
        .read_feed_page(&subscription, &wide_scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read under the wide scope is served");
    assert_eq!(
        wide.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![a.id, b.id],
        "both tenants' entries are present under a scope admitting both"
    );
}

// 10. A mark raised above a position refuses the page with
//     `CursorBeyondRetention`, and a first read over the same meter is still
//     served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mark_above_a_position_refuses_it_but_not_a_first_read() {
    // Ruling D7: this test's subject is the page's *read* of a mark, not the
    // sweep's *write* of one. `retention_sweep_tests::the_mark_raise_only_ever_raises`
    // pins the sweep's write, and Task 5's driven dispatch is what joins the
    // two through a real chunk drop; here the mark is raised directly (via
    // the sweep's own [`RAISE_MARKS_SQL`]) so this test can land before that
    // drive exists. This is not coverage of the retention interlock itself.
    let (h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let entry = common::entry(&meter, tenant, "feed-mark", Decimal::ONE);
    adapter
        .create_usage_record(entry.clone())
        .await
        .expect("the entry is accepted");
    let xact = common::xact_id_of(&h.pool, entry.id).await;
    let after = (xact, entry.id);

    raise_mark(
        &h.pool,
        meter.as_str(),
        xact + 1,
        Uuid::from_u128(u128::MAX),
    )
    .await;

    let err = adapter
        .read_feed_page(
            &subscription,
            &scope,
            FeedStart::After(encode_position(after.0, after.1)),
            None,
            10,
        )
        .await
        .expect_err("a mark above the position refuses the continuation");
    assert!(
        matches!(err, UsageCollectorPluginError::CursorBeyondRetention),
        "refused as CursorBeyondRetention: {err}"
    );

    let first_read = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is never refused on the retention floor, marks included");
    assert_eq!(
        first_read.entries.len(),
        1,
        "the meter's one entry is still served on a first read"
    );
}

// Mutation 4 (deleting step 6 while keeping step 3) is the most important
// single mutation in this task: if it does not red, one mark read is tested
// under a claim of two. The test above inserts its mark *before* the page
// read starts, so step 3's fast path alone already catches it, and that
// mutation would not red against it alone.
//
// This test raises the mark on a second connection strictly after step 3 has
// already run and found nothing, and strictly before the page's `COMMIT` — so
// only step 6, the autocommit re-check this mutation deletes, can catch it.
//
// **A deterministic gate, not a wall-clock race.** An earlier version of this
// test tried to land the window by racing `tokio::join!` against wall-clock
// completion order; measured over several runs it failed on the correct
// implementation roughly one round in twenty (the client-side instant a
// commit is acknowledged at is not a reliable proxy for server-side
// statement ordering at sub-millisecond separation) — a flaky test being
// exactly the "bad work" this task's instructions ask to avoid. Instead: an
// `ACCESS EXCLUSIVE` lock on `usage_records`, held open from a second
// connection *before* the read starts, blocks only step 4 (the page
// statement, which needs `ACCESS SHARE` on that table) — steps 2 and 3 touch
// neither `usage_records` nor this lock, so they always run to completion
// (step 3 finding no mark, since none exists yet) before the read can
// possibly reach the block. Polling `pg_stat_activity` for that block is the
// signal that steps 1-3 are done; only then is the mark raised and the lock
// released, so its commit is placed in the exact window every round, with no
// timing dependency at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mark_committed_during_the_page_transaction_is_still_caught() {
    let (h, adapter) = common::start_backend().await;
    let adapter = std::sync::Arc::new(adapter);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    for round in 0_u32..5 {
        let entry = common::entry(
            &meter,
            tenant,
            &format!("feed-mark-race-{round}"),
            Decimal::ONE,
        );
        adapter
            .create_usage_record(entry.clone())
            .await
            .unwrap_or_else(|err| panic!("round {round}: the entry is accepted: {err}"));
        let xact = common::xact_id_of(&h.pool, entry.id).await;
        let position = encode_position(xact, entry.id);

        let mut locker = h.pool.begin().await.expect("begin the locking transaction");
        sqlx::query("LOCK TABLE usage_records IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *locker)
            .await
            .expect("acquire the exclusive lock that gates step 4");

        let read_adapter = std::sync::Arc::clone(&adapter);
        let read_scope = scope.clone();
        let read_subscription = subscription.clone();
        let read_task = tokio::spawn(async move {
            read_adapter
                .read_feed_page(
                    &read_subscription,
                    &read_scope,
                    FeedStart::After(position),
                    None,
                    10,
                )
                .await
        });

        // Proof that steps 1-3 have already run: the read's backend is now
        // blocked waiting for the lock step 4 needs.
        wait_for_lock_wait(&h.pool).await;

        raise_mark(
            &h.pool,
            meter.as_str(),
            xact + 1,
            Uuid::from_u128(u128::MAX),
        )
        .await;

        locker
            .commit()
            .await
            .expect("release the lock, unblocking step 4");
        let result = read_task
            .await
            .expect("the spawned page read did not panic");

        assert!(
            matches!(
                result,
                Err(UsageCollectorPluginError::CursorBeyondRetention)
            ),
            "round {round}: the mark committed after step 3 had already run and found nothing, \
             and before the read's own COMMIT, so only step 6 (the authoritative, autocommit, \
             post-COMMIT re-check) could have caught it: {result:?}"
        );
    }
}

/// Polls until some backend on `pool` is blocked waiting for a table-level
/// lock, up to 2.5 s.
///
/// # Panics
///
/// If nothing is found waiting within the budget — the deterministic gate
/// [`a_mark_committed_during_the_page_transaction_is_still_caught`] depends on
/// failed to engage, which is a harness defect in that test rather than a
/// finding about the plugin.
async fn wait_for_lock_wait(pool: &sqlx::PgPool) {
    for _ in 0_u32..500 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .expect("read pg_stat_activity");
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "no backend blocked on the held lock within 2.5s; the deterministic gate did not engage"
    );
}

// 11. An entry visible to the snapshot but not settled is NOT delivered.
//     <-- Review Focus 5
//
// Completeness, and the one assertion that shows the horizon bound does work
// the snapshot does not already do. A snapshot sees rows from transactions
// above its `xmin` that committed before it was taken; those are exactly the
// entries that are visible and not settled, because the transaction at `xmin`
// can still commit a row at a *lower* position.
//
// Shape: writer A opens a transaction and inserts without committing, so it
// holds an id and pins the horizon. Writer B then inserts and commits, so its
// row is visible to any later snapshot and sits above A's id. A page read now
// must deliver neither — B's row is above the horizon — and the cursor it
// returns must be at or below A's id, so that A's row is still ahead of it
// when A commits.
//
// 20 rounds inside one test rather than repeated runs of one attempt: this
// project measured that 12 single-attempt runs found nothing where 20 rounds
// inside one test reproduced immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_visible_but_unsettled_entry_is_not_delivered_ahead_of_its_gap() {
    let (h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    // A warm-up write, fully committed before the race begins. Measured
    // necessary: without it, round 0's writer A (a raw INSERT held open) and
    // writer B (through the adapter) contend over creating this meter's very
    // first hypertable chunk, and B's write comes back `Transient` — a
    // harness hazard the completeness argument says nothing about, not the
    // subject under test.
    let warmup = common::entry(&meter, tenant, "gap-warmup", Decimal::ONE);
    adapter
        .create_usage_record(warmup)
        .await
        .expect("the warm-up entry establishes the meter's chunk before the race begins");

    for round in 0_u32..20 {
        let a = common::entry(&meter, tenant, &format!("gap-a-{round}"), Decimal::ONE);
        let b = common::entry(&meter, tenant, &format!("gap-b-{round}"), Decimal::ONE);

        // The head position before this round's two entries exist. Reading
        // from here rather than from `FeedStart::Oldest` keeps the assertion
        // below about *this round's* entries even once earlier rounds have
        // committed enough rows to fill a 10-row page on their own.
        let before = adapter
            .read_feed_page(
                &subscription,
                &scope,
                FeedStart::Oldest,
                None,
                MAX_PAGE_SIZE,
            )
            .await
            .expect("a baseline read is served")
            .next
            .expect("a live page carries a continuation");

        // Writer A: opened on its own connection and left uncommitted, so it
        // holds the lowest in-progress xact_id for as long as this round
        // needs it to.
        let mut tx_a = h.pool.begin().await.expect("begin writer A's transaction");
        insert_raw_within(&mut tx_a, &a).await;

        // Writer B: an ordinary write through the adapter under test. It
        // opens (and is assigned its xact_id) after A, and commits before the
        // page read below, so its row is visible to any snapshot taken now.
        adapter
            .create_usage_record(b.clone())
            .await
            .unwrap_or_else(|err| panic!("round {round}: writer B's entry is accepted: {err}"));

        let page = adapter
            .read_feed_page(&subscription, &scope, FeedStart::After(before), None, 10)
            .await
            .expect("the page is served");
        assert!(
            page.entries.iter().all(|e| e.id != a.id && e.id != b.id),
            "round {round}: neither writer A's entry (still open) nor writer B's (visible to \
             the snapshot but above the horizon A pins) may be delivered: {:?}",
            page.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        );
        let cursor_while_a_was_open = page.next.expect("a live page carries a continuation");

        tx_a.commit().await.expect("writer A commits");

        let resumed = adapter
            .read_feed_page(
                &subscription,
                &scope,
                FeedStart::After(cursor_while_a_was_open),
                None,
                10,
            )
            .await
            .expect("resuming from the pre-commit cursor is served");
        let ids: Vec<Uuid> = resumed.entries.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            vec![a.id, b.id],
            "round {round}: the cursor issued while writer A was still open must be strictly \
             behind A's entry, so resuming from it delivers A and then B, in feed order"
        );
    }
}

/// Insert `record` directly, within an already-open transaction, so a test
/// can hold it uncommitted across other work. The production write path
/// (`PgRecordStore::create`) always commits before returning, so this is the
/// one way a test can control when a write settles.
///
/// Mirrors `common::insert_raw_entry`'s column list, minus the columns this
/// suite never sets (`subject_id` / `subject_type`, `invalidates` /
/// `reason_code`): every entry this file writes is an ordinary measurement.
async fn insert_raw_within(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &usage_collector_sdk::UsageRecord,
) {
    sqlx::query(
        "INSERT INTO usage_records (id, tenant_id, gts_type_id, type_key, quantity, \
         window_start, window_end, resource_id, resource_type, idempotency_key, \
         invalidates, reason_code, origin, entry_type, accepted_at, metadata) \
         VALUES ($1, $2, $3, 1, $4, $5, $6, $7, $8, $9, NULL, NULL, $10, \
         'record'::usage_entry_type, $11, '{}'::jsonb)",
    )
    .bind(record.id)
    .bind(record.tenant_id)
    .bind(record.gts_type_id.as_str())
    .bind(record.quantity.as_decimal())
    .bind(record.window_start)
    .bind(record.window_end)
    .bind(record.resource_ref.resource_id())
    .bind(record.resource_ref.resource_type())
    .bind(record.idempotency_key.as_str())
    .bind(record.origin.as_str())
    .bind(record.accepted_at)
    .execute(&mut **tx)
    .await
    .expect("insert writer A's row directly, within its own open transaction");
}
