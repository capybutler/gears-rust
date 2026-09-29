#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! What the derived entry identity means **for storage**, against a live
//! `TimescaleDB`. Requires Docker.
//!
//! The derivation itself is the SDK's, and `id_tests.rs` pins its bytes; what
//! is asked here is the consequence the ledger has to honour. The id is a
//! `UUIDv5` over the 6-tuple
//! `(tenant_id, gts_type_id, idempotency_key, window_start, window_end,
//! entry_type)` (`cpt-cf-usage-collector-adr-record-identity-derivation`),
//! which is the six of the seven columns `usage_records_dedup_uniq` spans that
//! are identity inputs — the seventh, `type_key`, is the partition key a
//! hypertable UNIQUE must carry and is a function of `gts_type_id`, so it
//! separates no two rows. "Distinct ids" and "distinct rows" are therefore one
//! fact, and a point lookup addresses exactly one of them.
//!
//! `entry_type` is the sixth input, and the ADR is explicit about what it buys:
//! "An invalidation repeats its target's idempotency key, so the entry type is
//! the only input that keeps a measurement and its withdrawal apart." It is
//! also what gives every withdrawal of one target one identifier — two of them
//! collide on all six inputs — so a second withdrawal must surface as a same-id
//! content mismatch (a different `reason_code`) rather than being admitted as a
//! second entry. That is the last test here, and it is the ADR's own
//! confirmation item.

mod common;

use rust_decimal::Decimal;
use time::Duration;
use uuid::Uuid;

use usage_collector_sdk::{Invalidation, ReasonCode, UsageCollectorPluginError};

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;

/// Two entries differing **only** in `window_start` are two entries, not a
/// retry of one.
///
/// The covered period's start is one of the six inputs, so a stable
/// per-meter idempotency key covers many periods without collapsing them onto
/// one entry — which is the whole reason the derivation reads the period at
/// all rather than the scope and the key alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entries_differing_only_in_window_start_are_distinct_and_separately_addressable() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
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
        "window_start is one of the six dedup-identity inputs, so two entries \
         differing only in it derive distinct identifiers"
    );

    store
        .create(early.clone())
        .await
        .expect("create the early entry");
    store
        .create(late.clone())
        .await
        .expect("a distinct 6-tuple is a fresh insert, not a dedup hit");

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
    assert_eq!(got_early.quantity, early.quantity);
    let got_late = store
        .get(late.id, &scope)
        .await
        .expect("get the late entry");
    assert_eq!(got_late.window_start, late.window_start);
    assert_eq!(got_late.quantity, late.quantity);
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
    let store = common::record_store(&h);
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
        .expect("a distinct 6-tuple is a fresh insert");

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
    let store = common::record_store(&h);
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

/// `entry_type` is the sixth input to the derivation, and a withdrawal repeats
/// its target's idempotency key, so the entry type is the only input that keeps
/// the pair apart.
///
/// Two halves, and the second is the one that pays for the first:
///
/// 1. A faithful withdrawal shares five of the six inputs with its target —
///    tenant, meter, idempotency key and both period bounds — and departs in
///    the entry type alone, so the two ids differ and the ledger takes two
///    rows. Keyed on the other five alone, the withdrawal would collide with
///    the very entry it withdraws, be swallowed by `ON CONFLICT DO NOTHING`,
///    and come back as an `IdempotencyConflict` against its own target.
/// 2. A second withdrawal of the SAME target agrees on all six and so derives
///    the same id: the dedup UNIQUE sees a hit, and the store resolves
///    absorb-vs-conflict against the row already there. A byte-for-byte
///    identical resubmission is `Ok` (this crate's
///    `an_exact_retry_is_absorbed_and_returns_the_persisted_row` in
///    `records_ingest_integration_pg.rs` pins that half); this test gives the
///    second withdrawal a distinct `reason_code`, so the store answers
///    `IdempotencyConflict` against the row already holding the identity
///    rather than admitting a second entry
///    (`cpt-cf-usage-collector-adr-record-identity-derivation`, "Entry type is
///    an input", and its confirmation list).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_withdrawal_of_one_target_derives_the_same_id_and_a_content_mismatch_conflicts() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x00C0_FFF1);

    let target = common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0));
    let target = store.create(target).await.expect("create the target");

    // (1) The derived key: a different id from the target, and a fresh row.
    let withdrawal = common::withdrawal_of(&target);
    assert_ne!(
        withdrawal.id, target.id,
        "an invalidation and its target differ in their identifiers through the \
         entry type, and through nothing else: the withdrawal repeats the \
         target's idempotency key and covered period"
    );
    let stored = store
        .create(withdrawal)
        .await
        .expect("a faithful withdrawal of a resolvable target is accepted");
    assert_eq!(
        stored.invalidation.as_ref().map(|i| i.target),
        Some(target.id),
        "the withdrawal names the entry it withdraws"
    );

    // (2) A second withdrawal of the same target: the same six inputs, so one
    // identifier — `common::withdrawal_of` takes no idempotency-key argument at
    // all, because the key is the target's. Only the reason differs, and it is
    // no identity input, so the two are not canonically equal.
    let mut collided = common::withdrawal_of(&target);
    assert_eq!(
        collided.id, stored.id,
        "every withdrawal of one target agrees on all six inputs, so all of \
         them derive one identifier"
    );
    collided.invalidation = Some(Invalidation {
        target: target.id,
        reason: ReasonCode::new("second_withdrawal").expect("valid reason code"),
    });
    let err = store
        .create(collided)
        .await
        .expect_err("all six dedup attributes collide, and the two are not canonically equal");
    match err {
        UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing,
        } => {
            assert_eq!(idempotency_key, stored.idempotency_key.as_str());
            assert_eq!(
                existing.id, stored.id,
                "the conflict names the entry already holding the identity"
            );
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }

    // Two rows, not three: the colliding withdrawal was refused, not stored.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&h.pool)
        .await
        .expect("count rows");
    assert_eq!(
        rows, 2,
        "the colliding withdrawal must not have been admitted"
    );
}
