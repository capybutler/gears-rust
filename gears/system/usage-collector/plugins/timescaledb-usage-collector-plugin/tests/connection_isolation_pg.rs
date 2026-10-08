#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Every request-path connection runs at `READ COMMITTED`, whatever the server
//! defaults to (`docs/DESIGN.md` §3.5,
//! `docs/features/registration-schema-provisioning.md` `inst-pool-isolation`).
//! Requires Docker.
//!
//! **What is actually at stake.** The write path resolves a lost dedup slot by
//! reading the winning row back *inside the transaction that lost*
//! (`docs/DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-ingest-batch`). At
//! `READ COMMITTED` that read takes a fresh snapshot and sees the winner. Under
//! a deployer-set `default_transaction_isolation = repeatable read` it keeps
//! the transaction's original snapshot, the winner is invisible, and the loser
//! answers a `Transient` where DESIGN §3.3's `dedup-concurrent` row requires it
//! to resolve absorb-vs-conflict.
//!
//! **Why this suite asserts the GUC and not that race.** What the plugin
//! controls is the isolation level its own connections run at; the race's
//! outcome below that is `PostgreSQL`'s snapshot semantics, not this crate's. So
//! the property under test is the one the plugin can hold — every pooled
//! connection reads `read committed` — and it is observable with one query
//! rather than orchestrated concurrency that would be timing-dependent for no
//! extra coverage. The concurrent write path itself is covered by
//! `records_ingest_integration_pg`.
//!
//! The hostile default is set on the container the harness already started
//! rather than by starting a second one, the way `startup_durability_pg` does
//! for `fsync`. `default_transaction_isolation` is `sighup`-context, so
//! `ALTER SYSTEM` plus `pg_reload_conf()` reaches it.

mod common;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection as _, PgPool};

use timescaledb_usage_collector_plugin::infra::storage::pool::build_pool;

/// The hostile setting this suite deploys against, and the statement that sets
/// it. Kept as two literals rather than one `format!`: `sqlx::query` takes only
/// `&'static str`, and a hand-built statement here would need `AssertSqlSafe`
/// for no gain — the value is fixed.
const HOSTILE_ISOLATION: &str = "repeatable read";
const SET_HOSTILE_ISOLATION_SQL: &str =
    "ALTER SYSTEM SET default_transaction_isolation = 'repeatable read'";

/// Connect to the harness's database *without* going through [`build_pool`],
/// so none of the `-c` startup parameters [`build_pool`] applies are present.
///
/// This is what makes the control assertion below mean something: it reads the
/// server's own default, which a pool built by this crate deliberately no
/// longer inherits.
async fn raw_pool(
    cfg: &timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig,
) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect(cfg.database_url.expose())
        .await
        .expect("raw connect to the harness database")
}

/// Read one session's `default_transaction_isolation`.
async fn isolation_of(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT current_setting('default_transaction_isolation')")
        .fetch_one(pool)
        .await
        .expect("read default_transaction_isolation")
}

#[tokio::test]
async fn pooled_connections_run_read_committed_under_a_hostile_server_default() {
    let harness = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    // 1. Make the server hostile. `ALTER SYSTEM` cannot run inside a
    //    transaction block, so it is sent on its own rather than alongside the
    //    reload.
    sqlx::query(SET_HOSTILE_ISOLATION_SQL)
        .execute(&harness.pool)
        .await
        .expect("ALTER SYSTEM");
    sqlx::query("SELECT pg_reload_conf()")
        .execute(&harness.pool)
        .await
        .expect("reload");

    // 2. Control. A connection that did not come from `build_pool` must read
    //    the hostile default back. Without this the main assertion below could
    //    pass on a server that was never reconfigured at all — the reload is
    //    asynchronous, so "it says read committed" would prove nothing.
    //
    //    Note this cannot be `common::await_setting(&harness.pool, ..)` the way
    //    `startup_durability_pg` checks `fsync`: `harness.pool` *is* a
    //    `build_pool` pool, so it now forces this very setting and would read
    //    `read committed` forever. The raw pool is the only session that still
    //    sees the server's answer.
    let raw = raw_pool(&harness.cfg).await;
    common::await_setting(&raw, "default_transaction_isolation", HOSTILE_ISOLATION).await;

    // 3. The property. A pool this crate builds ignores all of that.
    let pool = build_pool(&harness.cfg)
        .await
        .expect("build_pool against a server defaulting to repeatable read");
    assert_eq!(
        isolation_of(&pool).await,
        "read committed",
        "a request-path connection must run at READ COMMITTED whatever the server \
         defaults to; the write path's conflict read-back is only correct there"
    );
}

#[tokio::test]
async fn the_feeds_explicit_begin_still_overrides_the_forced_default() {
    let harness = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let pool = build_pool(&harness.cfg).await.expect("build_pool");

    // The feed page opens `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`
    // because its horizon read must fix the snapshot the page statement then
    // runs under (`docs/DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-feed-page`).
    // Forcing the connection default to `read committed` must not reach it: a
    // per-transaction `BEGIN` outranks the connection default, and if it ever
    // stopped doing so the feed would silently lose its fixed snapshot and
    // serve pages that skip entries. That failure would be invisible at the
    // call site, which is why it is pinned here next to the change that could
    // cause it.
    let mut conn = pool.acquire().await.expect("acquire");
    let mut tx = conn
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .expect("begin the feed's transaction");
    let in_tx: String = sqlx::query_scalar("SELECT current_setting('transaction_isolation')")
        .fetch_one(&mut *tx)
        .await
        .expect("read transaction_isolation");
    assert_eq!(
        in_tx, "repeatable read",
        "the feed's explicit BEGIN must still fix its snapshot, despite the \
         connection default now being read committed"
    );
}
