#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Durable acknowledgement on the write transactions whose commit is
//! acknowledged to a caller (`docs/DESIGN.md` §3.5,
//! `docs/features/record-ingestion-idempotency.md` `inst-single-begin` and
//! `inst-batch-begin`). Requires Docker.
//!
//! §3.5: *"Every write transaction whose commit is acknowledged to a caller
//! runs `SET LOCAL synchronous_commit = on`, so an operator-level
//! `synchronous_commit` of `off` or `local` cannot weaken an
//! acknowledgement."* The half that is checkable without crashing a server is
//! the *"cannot"*: that the write transaction's own setting is `on` whatever
//! the operator left the server at.
//!
//! **Read back from inside the write transaction, not off the SQL text.** The
//! statement is issued by `record_store`'s private `begin_durable_write`, so a
//! test cannot hold the transaction open and query it. A `BEFORE INSERT` row
//! trigger on the ledger runs *inside* that transaction, on the ledger's own
//! `INSERT`, and records what `current_setting` answers there. Grepping the
//! statement text would pass against a backend that built the string and never
//! sent it.
//!
//! **Non-vacuous by construction.** Each case first turns the server's own
//! `synchronous_commit` off and reads that back, so the value the trigger
//! observes can only be `on` because the write path set it. Without that, both
//! assertions would pass against a plugin that issues nothing, since `on` is
//! `PostgreSQL`'s default.

mod common;

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

/// This suite's own tenant. `common` exports no tenant ids; each suite declares
/// the ones it needs, as `rollup_aggregate_integration_pg` does.
const TENANT: Uuid = Uuid::from_u128(0x5_C0FF);

/// The setting under test, and the value the write path owes whatever the
/// server is set to.
const SETTING: &str = "synchronous_commit";
const REQUIRED: &str = "on";

/// What the server is turned down to first, so a passing assertion cannot be
/// the default.
const WEAKENED: &str = "off";

/// A migrated container whose server-wide `synchronous_commit` is `off`, plus a
/// ledger trigger that records the setting each `INSERT` sees.
///
/// `ALTER SYSTEM` plus `pg_reload_conf()` is the mechanism `startup_durability_pg`
/// established for this lane, and the reload is read back through
/// [`common::await_setting`] rather than assumed. `synchronous_commit` is a
/// `user`-context setting that the pool does not set as a connection parameter,
/// so a reload reaches the pooled sessions that are already open as well as any
/// opened afterwards.
async fn setup() -> (common::TsHarness, PgRecordStore) {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    sqlx::query("ALTER SYSTEM SET synchronous_commit = off")
        .execute(&h.pool)
        .await
        .expect("ALTER SYSTEM");
    sqlx::query("SELECT pg_reload_conf()")
        .execute(&h.pool)
        .await
        .expect("reload");
    common::await_setting(&h.pool, SETTING, WEAKENED).await;

    // The probe. A `BEFORE INSERT` row trigger is copied onto each chunk by
    // TimescaleDB, so it fires for a hypertable insert; it writes to an
    // ordinary table, inside whatever transaction the ledger write opened, and
    // returns the row unchanged so the write itself is unaffected.
    sqlx::query("CREATE TABLE durability_probe (observed text NOT NULL)")
        .execute(&h.pool)
        .await
        .expect("probe table");
    sqlx::query(
        "CREATE FUNCTION record_commit_mode() RETURNS trigger AS $$ \
         BEGIN \
           INSERT INTO durability_probe (observed) \
           VALUES (current_setting('synchronous_commit')); \
           RETURN NEW; \
         END; $$ LANGUAGE plpgsql",
    )
    .execute(&h.pool)
    .await
    .expect("probe function");
    sqlx::query(
        "CREATE TRIGGER durability_probe_trg BEFORE INSERT ON usage_records \
         FOR EACH ROW EXECUTE FUNCTION record_commit_mode()",
    )
    .execute(&h.pool)
    .await
    .expect("probe trigger");

    let store = common::record_store(&h);
    (h, store)
}

/// Everything the probe recorded, in insertion order.
async fn observed(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT observed FROM durability_probe")
        .fetch_all(pool)
        .await
        .expect("read the probe back")
}

/// An ordinary measurement on this suite's tenant and meter.
fn entry(idem: &str) -> usage_collector_sdk::UsageRecord {
    let m = common::meter(common::VCPU_METER);
    common::entry(&m, TENANT, idem, Decimal::ONE)
}

/// Assert the probe saw one row per written entry and `on` for every one.
///
/// The count matters as much as the value: an empty probe would make a
/// `iter().all(...)` assertion vacuously true, which is how a trigger that
/// never fired would read as a pass.
fn assert_durable(rows: &[String], expected_writes: usize, path: &str) {
    assert_eq!(
        rows.len(),
        expected_writes,
        "{path}: the probe must have fired once per written row, or the value below \
         says nothing. got: {rows:?}"
    );
    assert!(
        rows.iter().all(|value| value == REQUIRED),
        "{path}: a write transaction whose commit is acknowledged to a caller must \
         run `SET LOCAL synchronous_commit = on`, so an operator-level setting \
         cannot weaken an acknowledgement. The server is \
         at `{WEAKENED}` and the ledger insert saw: {rows:?}"
    );
}

/// The server really is weakened, so the two assertions below are about the
/// write path rather than about a default.
///
/// Separate from them because it is the premise both rest on: were it to stop
/// holding, both would pass against a plugin that issues nothing, and neither
/// would say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_under_test_is_weakened_so_the_other_cases_are_not_vacuous() {
    let (h, _store) = setup().await;
    let session: String = sqlx::query_scalar("SELECT current_setting($1)")
        .bind(SETTING)
        .fetch_one(&h.pool)
        .await
        .expect("read the setting back");
    assert_eq!(
        session, WEAKENED,
        "an ordinary session on this harness must see the weakened server setting"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_single_row_write_transaction_forces_synchronous_commit() {
    let (h, store) = setup().await;

    store
        .create(entry("durable-single"))
        .await
        .expect("a fresh entry is admitted");

    assert_durable(&observed(&h.pool).await, 1, "single-row path");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_batch_write_transaction_forces_synchronous_commit() {
    let (h, store) = setup().await;

    let outcomes = store
        .create_batch(vec![entry("durable-batch-a"), entry("durable-batch-b")])
        .await
        .expect("a batch of two fresh entries is admitted");
    assert_eq!(outcomes.len(), 2, "one outcome per input entry");
    for outcome in &outcomes {
        outcome.as_ref().expect("each entry is admitted");
    }

    assert_durable(&observed(&h.pool).await, 2, "batch path");
}
