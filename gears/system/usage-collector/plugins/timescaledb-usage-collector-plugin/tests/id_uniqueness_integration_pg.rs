#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! What the derived entry identity means **for storage**, against a live
//! `TimescaleDB`. Requires Docker.
//!
//! The derivation itself is the SDK's, and `id_tests.rs` pins its bytes; what
//! is asked here is the consequence the ledger has to honour. The id is a
//! `UUIDv5` over the 5-tuple
//! `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`
//! (`cpt-cf-usage-collector-adr-record-identity-derivation`), which is the same
//! five columns `usage_records_dedup_uniq` spans — so "distinct ids" and
//! "distinct rows" are one fact, and a point lookup addresses exactly one of
//! them.
//!
//! `entry_type` is deliberately **not** an input, and the ADR is explicit about
//! what that buys: an invalidation submitted under its target's own key
//! collides on all five and must surface as a same-key content mismatch rather
//! than being admitted as a second entry. That is the last test here, and it is
//! the ADR's own confirmation item.

mod common;

use rust_decimal::Decimal;
use time::Duration;
use uuid::Uuid;

use usage_collector_sdk::UsageCollectorPluginError;

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;

/// Two entries differing **only** in `window_start` are two entries, not a
/// retry of one.
///
/// The covered period's start is one of the five inputs, so a stable
/// per-meter idempotency key covers many periods without collapsing them onto
/// one entry — which is the whole reason the derivation reads five inputs and
/// not three.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entries_differing_only_in_window_start_are_distinct_and_separately_addressable() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x00C0_FFEE);
    let scope = common::tenant_scope(tenant);

    // Same tenant, same meter, same key, same period *end*. Only the start
    // moves, and it moves by an hour rather than by a microsecond so the two
    // are unambiguously different periods rather than a precision artefact.
    let end = common::fixture_window_end();
    let early = common::entry_over(
        &meter,
        tenant,
        "idem-shared",
        Decimal::ONE,
        common::fixture_window_start(),
        end,
    );
    let late = common::entry_over(
        &meter,
        tenant,
        "idem-shared",
        Decimal::new(2, 0),
        common::fixture_window_start() + Duration::hours(1),
        end,
    );

    assert_ne!(
        early.id, late.id,
        "window_start is one of the five dedup-identity inputs, so two entries \
         differing only in it derive distinct identifiers"
    );

    store
        .create(early.clone())
        .await
        .expect("create the early entry");
    store
        .create(late.clone())
        .await
        .expect("a distinct 5-tuple is a fresh insert, not a dedup hit");

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_records WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant)
    .bind("idem-shared")
    .fetch_one(&h.pool)
    .await
    .expect("count rows for the shared key");
    assert_eq!(
        rows, 2,
        "one idempotency key over two periods must persist as two rows"
    );

    // And each id addresses its own row: the point lookup is surgical, which is
    // what "distinct ids" has to mean to be worth anything.
    let got_early = store
        .get(early.id, &scope)
        .await
        .expect("get the early entry");
    assert_eq!(got_early.window_start, early.window_start);
    assert_eq!(got_early.value, early.value);
    let got_late = store
        .get(late.id, &scope)
        .await
        .expect("get the late entry");
    assert_eq!(got_late.window_start, late.window_start);
    assert_eq!(got_late.value, late.value);
}

/// The same, for the period's **end**.
///
/// Written out rather than folded into the test above with a loop, because the
/// two bounds enter the pre-image in start-then-end order and the failure this
/// catches is a derivation that reads one bound twice or reads them
/// order-blind — which a loop over "vary one field" would not distinguish from
/// a derivation that reads both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entries_differing_only_in_window_end_are_distinct() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x00C0_FFEF);

    let start = common::fixture_window_start();
    let short = common::entry_over(
        &meter,
        tenant,
        "idem-shared-end",
        Decimal::ONE,
        start,
        common::fixture_window_end(),
    );
    let long = common::entry_over(
        &meter,
        tenant,
        "idem-shared-end",
        Decimal::new(2, 0),
        start,
        common::fixture_window_end() + Duration::hours(1),
    );

    assert_ne!(
        short.id, long.id,
        "window_end is a dedup-identity input too"
    );

    store.create(short).await.expect("create the short period");
    store
        .create(long)
        .await
        .expect("a distinct 5-tuple is a fresh insert");

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_records WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant)
    .bind("idem-shared-end")
    .fetch_one(&h.pool)
    .await
    .expect("count rows for the shared key");
    assert_eq!(rows, 2, "two periods, two rows");
}

/// A point event (`window_start == window_end`) derives an identity like any
/// other entry, and is a different entry from a period sharing its end.
///
/// The derivation needs no separate case for a zero-length period, and this is
/// where that is observable: the two rows below differ only in that one has a
/// start an hour earlier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_point_event_is_a_distinct_entry_from_a_period_sharing_its_end() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x00C0_FFF0);
    let scope = common::tenant_scope(tenant);

    let instant = common::fixture_window_end();
    let point = common::entry_over(&meter, tenant, "idem-point", Decimal::ONE, instant, instant);
    let period = common::entry_over(
        &meter,
        tenant,
        "idem-point",
        Decimal::new(2, 0),
        common::fixture_window_start(),
        instant,
    );

    assert_ne!(point.id, period.id);
    let point_id = point.id;
    store
        .create(point)
        .await
        .expect("a zero-length period is a point event, not an error");
    store.create(period).await.expect("create the period");

    let stored = store
        .get(point_id, &scope)
        .await
        .expect("get the point event");
    assert_eq!(
        stored.window_start, stored.window_end,
        "a point event's bounds must survive the round trip equal"
    );
}

/// `entry_type` is **not** an input to the derivation, and the idempotency key
/// is what separates an invalidation from its target.
///
/// Two halves, and the second is the one that pays for the first:
///
/// 1. A faithful withdrawal shares four of the five inputs with its target —
///    tenant, meter, and both period bounds — and carries its own key, so the
///    two ids differ.
/// 2. Reuse the target's key and all five collide. Since the kind is not an
///    input, the two derive **one** identifier, the dedup UNIQUE sees a hit,
///    and the store answers `IdempotencyConflict` against the target rather
///    than admitting a second entry. Admitting the kind into the derivation is
///    exactly what would turn this loud rejection into two silent rows
///    (`cpt-cf-usage-collector-adr-record-identity-derivation`, "Entry type is
///    excluded", and its confirmation list).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_type_is_not_an_input_so_a_reused_key_collides_instead_of_forking() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x00C0_FFF1);

    let target = common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0));
    let target = store.create(target).await.expect("create the target");

    // (1) Its own key: a different id, and a second row.
    let withdrawal = common::withdrawal_of(&target, "idem-withdrawal");
    assert_ne!(
        withdrawal.id, target.id,
        "an invalidation and its target differ in their identifiers for one reason only: \
         the idempotency key"
    );
    let stored = store
        .create(withdrawal)
        .await
        .expect("a withdrawal under its own key is a fresh entry");
    assert_eq!(
        stored.invalidation.as_ref().map(|i| i.target),
        Some(target.id),
        "the withdrawal names the entry it withdraws"
    );

    // (2) The target's key: one identifier, and a conflict.
    let collided = common::withdrawal_of(&target, "idem-target");
    assert_eq!(
        collided.id, target.id,
        "the kind is not an input, so a withdrawal reusing its target's key derives the \
         target's own identifier"
    );
    let err = store
        .create(collided)
        .await
        .expect_err("all five dedup attributes collide, and the two are not canonically equal");
    match err {
        UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing_id,
        } => {
            assert_eq!(idempotency_key, "idem-target");
            assert_eq!(
                existing_id, target.id,
                "the conflict names the entry already holding the 5-tuple"
            );
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }

    // Two rows, not three: the collided withdrawal was refused, not stored.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&h.pool)
        .await
        .expect("count rows");
    assert_eq!(
        rows, 2,
        "the collided withdrawal must not have been admitted"
    );
}
