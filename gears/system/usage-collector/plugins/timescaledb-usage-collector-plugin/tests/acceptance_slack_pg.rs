#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! The write path's acceptance-slack guard (`docs/DESIGN.md` §3.6
//! `cpt-cf-uc-plugin-seq-ingest-dedup`), at a narrow slack. Requires Docker.
//!
//! Every other pg suite runs at `common::HARNESS_ACCEPTANCE_SLACK_SECS`, under
//! which the guard admits everything. This suite is the only place its refusal
//! runs.

mod common;

use rust_decimal::Decimal;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use usage_collector_sdk::{UsageCollectorPluginError, UsageRecord};

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

/// Narrow enough that a fixture instant is far outside it, wide enough that a
/// clock-relative entry is comfortably inside on a loaded CI machine.
const NARROW_SLACK_SECS: u64 = 120;

/// This suite's own tenant. `common` exports no tenant ids; each suite declares
/// the ones it needs, as `rollup_aggregate_integration_pg` does.
const TENANT: Uuid = Uuid::from_u128(0x5_1AC);

/// `common::bring_up_with` plus `common::record_store`, at a narrow slack.
///
/// The slack is a fourth parameter on `bring_up_with` rather than a second
/// builder, so one function still owns the harness config, and
/// `common::record_store` reads it back off the harness rather than being told
/// it a second time.
async fn setup() -> (common::TsHarness, PgRecordStore) {
    let h = common::bring_up_with(30, 2, 16, NARROW_SLACK_SECS)
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    (h, store)
}

/// An entry at `accepted_at`, on this suite's tenant and meter.
///
/// **No `common::rederive` call.** `accepted_at` is not one of the six identity
/// inputs `derive_usage_record_id` takes, so overriding it does not move the
/// `id` and re-deriving would be a no-op that reads as though it were needed.
fn entry_accepted_at(idem: &str, accepted_at: OffsetDateTime) -> UsageRecord {
    let m = common::meter(common::VCPU_METER);
    UsageRecord {
        accepted_at,
        ..common::entry(&m, TENANT, idem, Decimal::ONE)
    }
}

/// A displacement far outside [`NARROW_SLACK_SECS`], so a test's own scheduling
/// latency cannot move a fixture across the boundary in either direction.
fn seconds_outside_the_slack() -> Duration {
    Duration::seconds(i64::try_from(NARROW_SLACK_SECS).expect("fits i64") * 10)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_accepted_now_is_admitted() {
    // The paired positive. An assertion that something is refused is vacuous
    // against a backend that refuses everything, and three of the four tests
    // below assert a refusal.
    let (_h, store) = setup().await;
    store
        .create(entry_accepted_at("slack-fresh", OffsetDateTime::now_utc()))
        .await
        .expect("an entry accepted now is admitted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_accepted_too_long_ago_is_refused_as_transient() {
    let (_h, store) = setup().await;
    let stale = OffsetDateTime::now_utc() - seconds_outside_the_slack();
    let err = store
        .create(entry_accepted_at("slack-old", stale))
        .await
        .expect_err("a stale acceptance must be refused");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "the refusal is retryable, so the host can re-stamp and retry: {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_accepted_too_far_ahead_is_refused_as_transient() {
    // `docs/DESIGN.md` §3.6: within the slack of the statement's own
    // `statement_timestamp()`, "in either direction". A one-sided predicate
    // passes every past-dated test and admits an entry from next year, which
    // would order ahead of everything real in acceptance terms.
    let (_h, store) = setup().await;
    let ahead = OffsetDateTime::now_utc() + seconds_outside_the_slack();
    let err = store
        .create(entry_accepted_at("slack-ahead", ahead))
        .await
        .expect_err("a future acceptance must be refused");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_retry_of_a_stored_identity_is_refused_rather_than_absorbed() {
    // `docs/DESIGN.md` §3.6: "**The guard's verdict takes precedence**: a row
    // not admitted is `Transient` … even when its identity exists".
    //
    // This is the ordering rule between two independent outcomes, and it has
    // its own test because a backend that resolved the conflict first would
    // answer `IdempotencyConflict` or absorb, and would pass every other
    // assertion in this file. The retry carries the same six identity inputs as
    // the stored entry, since only `accepted_at` differs.
    let (_h, store) = setup().await;
    let fresh = entry_accepted_at("slack-precedence", OffsetDateTime::now_utc());
    store.create(fresh.clone()).await.expect("stored");

    let stale_retry = UsageRecord {
        accepted_at: OffsetDateTime::now_utc() - seconds_outside_the_slack(),
        ..fresh
    };
    let err = store
        .create(stale_retry)
        .await
        .expect_err("the guard decides before the identity does");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "a stale retry of a stored identity is Transient, not a conflict or an absorb: {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_refuses_only_its_stale_rows() {
    // Per-row `admitted`, per §3.6 `cpt-cf-uc-plugin-seq-ingest-batch`: "a
    // conflict or rejection on one record never fails the others".
    let (_h, store) = setup().await;
    let fresh = entry_accepted_at("slack-batch-fresh", OffsetDateTime::now_utc());
    let stale = entry_accepted_at(
        "slack-batch-stale",
        OffsetDateTime::now_utc() - seconds_outside_the_slack(),
    );
    let out = store
        .create_batch(vec![fresh, stale])
        .await
        .expect("the call itself succeeds; the rejection is per row");
    assert!(out[0].is_ok(), "the fresh row is admitted: {:?}", out[0]);
    assert!(
        matches!(out[1], Err(UsageCollectorPluginError::Transient { .. })),
        "the stale row alone is refused, in its input position: {:?}",
        out[1]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_row_leaves_no_ledger_entry_and_no_transaction_id() {
    // The refusal is not merely reported: the statement's `WHERE admitted` is
    // what keeps the row out of the ledger, so nothing is written and no
    // `xact_id` is consumed by it. Asserted directly, because a path that
    // inserted the row and then reported `Transient` would pass every
    // assertion above while leaving an entry the feed would serve.
    let (h, store) = setup().await;
    let stale = entry_accepted_at(
        "slack-no-row",
        OffsetDateTime::now_utc() - seconds_outside_the_slack(),
    );
    let id = stale.id;
    store
        .create(stale)
        .await
        .expect_err("a stale acceptance must be refused");

    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(id)
        .fetch_one(&h.pool)
        .await
        .expect("count the refused entry's rows");
    assert_eq!(stored, 0, "a refused entry is not written at all");
}
