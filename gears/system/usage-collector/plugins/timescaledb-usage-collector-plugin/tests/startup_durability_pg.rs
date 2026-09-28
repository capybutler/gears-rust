#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The startup durability checks (`docs/DESIGN.md` §3.5,
//! `docs/features/registration-schema-provisioning.md` `inst-pool-fsync` and
//! `inst-pool-full-page-writes`). Requires Docker.
//!
//! The admit path is covered by every other pg suite, since `build_pool` runs
//! these checks on every container this lane starts. What is covered only here
//! is the refusal.
//!
//! Each case flips the setting on the container the harness already started
//! rather than starting a second one with `-c <setting>=off`. Both settings are
//! `sighup`-context, so `ALTER SYSTEM` plus `pg_reload_conf()` reaches them;
//! confirmed against `timescale/timescaledb:2.29.2-pg18` before this suite was
//! written, on an already-open session as well as a fresh one. The reload is
//! still read back through [`common::await_setting`] rather than assumed.

mod common;

use sqlx::AssertSqlSafe;

use timescaledb_usage_collector_plugin::infra::storage::pool::build_pool;

/// Turn `setting` off on the harness's server and assert `build_pool` refuses
/// it, naming the setting and the value it read.
async fn refuses_when_setting_is_off(setting: &str) {
    let harness = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    // `ALTER SYSTEM` takes an identifier, not a bind parameter, so the setting
    // name is interpolated. `AssertSqlSafe` is sound here because the only
    // values reaching it are the two literals the tests below pass. It cannot
    // run inside a transaction block either, so it is sent on its own rather
    // than alongside the reload.
    sqlx::query(AssertSqlSafe(format!("ALTER SYSTEM SET {setting} = off")))
        .execute(&harness.pool)
        .await
        .expect("ALTER SYSTEM");
    sqlx::query("SELECT pg_reload_conf()")
        .execute(&harness.pool)
        .await
        .expect("reload");
    // The reload is asynchronous, so read the setting back rather than assuming
    // it took; a test that built the pool too early would pass for the wrong
    // reason.
    common::await_setting(&harness.pool, setting, "off").await;

    let err = build_pool(&harness.cfg)
        .await
        .expect_err("a server with the setting off must not yield a pool");
    let message = err.to_string();
    assert!(
        message.contains(setting) && message.contains("off"),
        "the refusal must name the setting and its value, got: {message}"
    );
}

#[tokio::test]
async fn starting_against_a_server_with_fsync_off_fails() {
    refuses_when_setting_is_off("fsync").await;
}

#[tokio::test]
async fn starting_against_a_server_with_full_page_writes_off_fails() {
    refuses_when_setting_is_off("full_page_writes").await;
}
