#![cfg(feature = "postgres")]
// Shared across test binaries: `mod common;` compiles a private copy into each
// one, so a helper another binary uses is `dead_code` in this one. These
// helpers also panic on invalid test input by design.
//
// The allowance is earned by that sharing and by nothing else — `dead_code`
// hides a helper with no callers at all just as well, which is how
// `insert_raw_usage_record` kept an `INSERT` naming columns the table does not
// have, and a doc citing a foreign key the schema does not have, until Task 14
// deleted it. Before adding a helper here, check it has a caller somewhere;
// the lint will not.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]
//! Shared `TimescaleDB` testcontainer harness. Requires Docker.

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use sqlx::PgPool;
use testcontainers::core::WaitFor;
use testcontainers::core::logs::LogSource;
use testcontainers::core::wait::LogWaitStrategy;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_odata::ast;
use uuid::Uuid;

use usage_collector_sdk::{
    EntryType, IdempotencyKey, Invalidation, MeterTypeId, ReasonCode, RecordOrigin, ResourceRef,
    UsageQuantity, UsageRecord, derive_usage_record_id,
};

use timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig;
use timescaledb_usage_collector_plugin::domain::adapter::StorageAdapter;
use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::pool::{
    MIGRATOR, apply_post_migration_setup, build_pool,
};
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

pub struct TsHarness {
    pub pool: PgPool,
    pub cfg: TimescaleDbPluginConfig,
    _container: ContainerAsync<GenericImage>,
}

/// How many containers [`bring_up_with`] will burn through before giving up,
/// and how long it waits before starting the next one.
///
/// Three, because the failures it absorbs are contention against the Docker
/// daemon and a third attempt has never been needed; the backoff is there
/// because an immediate restart re-enters the contention that just lost.
const CONTAINER_ATTEMPTS: u32 = 3;
const CONTAINER_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(300);

/// How many times [`bring_up_with`] tries to open the pool against **one**
/// container, and how long it waits between tries - 10 seconds per container,
/// 30 seconds across all three.
///
/// Stated rather than inlined as a bare `0..20` because it is the only thing
/// that absorbs a server which has announced itself but is not yet accepting
/// connections, so its size decides whether the suite is flaky under load.
const CONNECT_ATTEMPTS: u32 = 20;
const CONNECT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// The acceptance slack every shared-harness backend runs at.
///
/// **Why it is not the published default of 120 s.** The write path refuses a
/// row whose `accepted_at` is further than this from the INSERT's own
/// `statement_timestamp()` (`docs/DESIGN.md` §3.6). Every fixture in this lane
/// is dated: [`fixture_window_end`] is 2023-11-14, and the SDK contract suite's
/// `CONTRACT_ACCEPTED_AT` is 2026-01-01. Both are fixed deliberately —
/// `server-field-round-trip` compares a read against a literal and
/// `latest-tie-break` varies `accepted_at` to rank entries, and neither works
/// against a clock — and the SDK's is a `const` this crate cannot reach.
///
/// So the shared harness widens the operator setting rather than re-dating the
/// fixtures. **The consequence is that the shared run exercises the guard's
/// admit path only**; its refusal is covered by `acceptance_slack_pg.rs`, which
/// builds its own backend at a narrow slack. A green run in this lane is not
/// evidence about the refusal.
///
/// A hundred years, which is far wider than any fixture's distance from the
/// clock, and that is the whole of why it is this number. It is deliberately
/// **not** described as the largest a deployment could configure: the interval
/// ceiling `TimescaleDbPluginConfig::validate` applies is private to `config`,
/// nothing checks this constant against it — `bring_up_with` deserializes the
/// config and never calls `validate`, which only `init` does — so a claim about
/// that bound here would be one nothing could keep true.
///
/// In production nothing carries a stale `accepted_at`: it is stamped by the
/// Ingestion Gateway when it accepts the entry, and `origin = backfill` names
/// the ingestion path while the old period lives in the covered-period bounds.
pub const HARNESS_ACCEPTANCE_SLACK_SECS: u64 = 100 * 365 * 86_400;

pub async fn bring_up() -> anyhow::Result<TsHarness> {
    // Default pool bounds and statement timeout (mirrors the config defaults),
    // and the wide acceptance slack this lane's dated fixtures need.
    bring_up_with(30, 2, 16, HARNESS_ACCEPTANCE_SLACK_SECS).await
}

/// Like [`bring_up`] but with an explicit request-path `statement_timeout` (secs),
/// pool bounds, and acceptance slack. The first three are used to assert the
/// init path does not leak a modified `statement_timeout` onto pooled
/// connections: pass a value distinct from any the init path might set, and a
/// small fixed pool so every connection can be inspected. The fourth is
/// `acceptance_slack_pg`'s: it is a parameter here rather than a second builder
/// so one function still owns the harness config.
pub async fn bring_up_with(
    statement_timeout_secs: u64,
    pool_size_min: u32,
    pool_size_max: u32,
    feed_acceptance_slack_secs: u64,
) -> anyhow::Result<TsHarness> {
    // The tag lives in `test_containers::TIMESCALEDB_TAG`; keep that constant
    // in sync with `TimescaleDbSidecar.IMAGE` in `testing/e2e/lib/sidecars.py`.
    // A skew means these migrations are validated against a different
    // PostgreSQL major than E2E runs.
    // A closure, not a value: `ContainerRequest` is consumed by `start()` and is
    // not `Clone`, so the retry below needs to build a fresh one per attempt.
    let image = || {
        test_containers::timescaledb()
            // Wait for the **second** "ready", across **both** streams. Both
            // halves of that are load-bearing.
            //
            // *Why the second.* This image announces readiness twice, and only
            // the second server is one a test can reach. `docker-entrypoint.sh`
            // runs a temporary bootstrap server for initdb with
            // `-c listen_addresses=''` (entrypoint line 297), i.e. **unix
            // socket only**; that one announces itself first. The image then
            // runs `/docker-entrypoint-initdb.d/001_timescaledb_tune.sh` - which
            // is also what makes `work_mem` 7837kB rather than the compiled
            // 4 MB - stops the bootstrap server, and starts the real one, which
            // announces itself again. Measured on
            // `timescale/timescaledb:2.29.2-pg18`: the bootstrap server at
            // `21:42:21.855`, the tune script, `PostgreSQL init process
            // complete`, then the real server at `21:42:22.498`. Waiting for
            // the first hands back a container whose published TCP port
            // refuses connections, leaving `build_pool`'s retry budget below as
            // the only thing between this suite and `pool connect failed`.
            //
            // *Why `BothStd`.* The two lines are not on the same stream as the
            // Docker API frames them: measured against this exact API,
            // `LogSource::StdErr` with `times(2)` never fires (45 s timeout),
            // while `BothStd` with `times(2)` returns in ~1.2 s and `BothStd`
            // with `times(3)` never fires - so there are exactly two in total
            // and they are split across the streams. Do not "tighten" this to
            // `stderr`; it was tried, and it hangs.
            //
            // Every container here is freshly created, so the count is always
            // exactly 2. A reused volume would skip initdb and log it once,
            // which is one more reason this harness never reuses one.
            .with_wait_for(WaitFor::log(
                LogWaitStrategy::new(
                    LogSource::BothStd,
                    "database system is ready to accept connections",
                )
                .with_times(2),
            ))
            .with_env_var("POSTGRES_USER", "user")
            .with_env_var("POSTGRES_PASSWORD", "pass")
            .with_env_var("POSTGRES_DB", "app")
            // Cap the shared-memory segment. This lane starts one container
            // per integration test and nextest runs them at
            // `test-threads = num_cpus`, so the peak is (num_cpus) live
            // servers at once. Each one runs
            // `/docker-entrypoint-initdb.d/001_timescaledb_tune.sh`, which
            // sizes `shared_buffers` from **host** RAM and has no idea it has
            // siblings: measured at `1959MB` on this 7.83 GB box, i.e. the
            // usual quarter of total RAM. Every container believing it owns
            // the machine is fine at one container and is N x that on a wide
            // runner.
            //
            // `SHOW shared_buffers` was read off a live container both ways:
            // `1959MB` with no override, `256MB` with the argument below, and
            // the readiness message still appears exactly twice, so the wait
            // strategy above is unaffected. `work_mem` stays at the tuned
            // `7837kB` either way - the argument overrides the one setting it
            // names and leaves the rest of the tune script's work in place.
            //
            // 256 MB is twice PostgreSQL's own compiled default, so no test
            // here has less headroom than it would get from a stock server;
            // the largest fixture in the suite is a hundred rows.
            //
            // Set at THIS call site rather than in `test_containers::
            // timescaledb()`: the concurrency that makes the default bite is
            // this lane's, and the shared helper hands back a bare
            // `GenericImage` precisely so callers supply their own runtime
            // arguments.
            .with_cmd(["postgres", "-c", "shared_buffers=256MB"])
    };
    // Start a container and connect to it, and treat **the whole of that** as
    // one attempt that may be retried with a fresh container.
    //
    // Three failure modes were measured on a fully loaded run of this suite,
    // and they are all the same underlying thing - the Docker daemon's port
    // publication racing a container that is already running - so they are
    // handled together rather than one at a time:
    //
    // 1. `get_host_port_ipv4` answers `container '<id>' does not expose port
    //    5432/tcp`. Measured at ~11 occurrences across 6 full runs of ~264
    //    containers each, i.e. under 1%.
    // 2. `start()` exceeds its startup timeout waiting for a readiness message
    //    a loaded box is slow to produce.
    // 3. **The published port answers, and an HTTP server is behind it.**
    //    Observed once as `pool connect failed … UnexpectedEof "expected to
    //    read 1414811696 bytes, got 47 bytes at EOF"`. Decoded against the
    //    `sqlx-postgres` this workspace locks (0.9.0):
    //    `connection/stream.rs` reads a five-byte header, requires byte 0 to
    //    be a valid `BackendMessageFormat` (`message/mod.rs`, or the error
    //    would read `unknown message type` instead), takes bytes 1..5 as
    //    `message_len`, and reports `expected_len = message_len + 1`. So the
    //    length prefix on the wire was `1414811695 = 0x5454502F`, the ASCII
    //    bytes `TTP/`, and byte 0 was `b'H'` (`CopyOutResponse`) - the only
    //    valid format byte that precedes `TTP/` in a real payload. **The first
    //    five bytes were `HTTP/`**, and
    //    `HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n` is exactly
    //    the 47 bytes the error reports.
    //
    //    So the peer was an HTTP server - a Docker Desktop port-forwarder or
    //    API endpoint, or another local service holding that ephemeral port -
    //    and not a Postgres that was merely slow. It happened in the one run
    //    of six that also retried a container, which ties it to the same race.
    //    On a recurrence, that is a checkable culprit class: find what is
    //    listening on the port and whether it speaks HTTP.
    //
    // (3) is why the pool connect is **inside** this loop rather than after it:
    // no amount of retrying `build_pool` against a wrong port can help, because
    // the port stays wrong. Discarding the container and starting another is
    // the only thing that can, and it is what the earlier shape - retry the
    // port lookup, then retry the pool separately for 30 s - could not do.
    //
    // Bounded at three containers. The two `start()` / port arms return the
    // last attempt's error **unchanged**, so an image that genuinely does not
    // expose 5432 still fails with the message that says so; the pool arm
    // wraps, because "every one of three containers refused a connection" is
    // itself the diagnosis - but it carries the underlying `sqlx` error
    // verbatim, which is how failure mode 3 above was decoded at all. That is
    // also why this is here rather than `nextest --retries`: a blanket retry
    // cannot tell a harness failure from an assertion failure, and would mask
    // the second.
    //
    // The backoff is not decoration - the stated cause is contention, so an
    // immediate restart re-enters exactly what just lost.
    //
    // **The messages below land on the stderr of a test that then passes**,
    // which nextest captures and discards. A rate rising from 1-in-100 to
    // 1-in-3 is therefore invisible until an attempt-3 failure. To see it:
    // `cargo nextest run … --success-output immediate | grep -c "starting
    // another"`.
    let mut brought_up: Option<(
        ContainerAsync<GenericImage>,
        PgPool,
        TimescaleDbPluginConfig,
    )> = None;
    for attempt in 1..=CONTAINER_ATTEMPTS {
        let last = attempt == CONTAINER_ATTEMPTS;
        let container = match image().start().await {
            Ok(container) => container,
            Err(err) if last => return Err(err.into()),
            Err(err) => {
                eprintln!(
                    "timescaledb container failed to start (attempt {attempt}/\
                     {CONTAINER_ATTEMPTS}): {err}; starting another"
                );
                tokio::time::sleep(CONTAINER_RETRY_BACKOFF).await;
                continue;
            }
        };
        let port = match container.get_host_port_ipv4(5432).await {
            Ok(port) => port,
            Err(err) if last => return Err(err.into()),
            Err(err) => {
                eprintln!(
                    "timescaledb container came up without a published port (attempt \
                     {attempt}/{CONTAINER_ATTEMPTS}): {err}; starting another"
                );
                drop(container);
                tokio::time::sleep(CONTAINER_RETRY_BACKOFF).await;
                continue;
            }
        };

        // The test container serves no TLS; `sslmode=disable` is the deliberate
        // opt-out that `build_pool` honors (production DSNs without an explicit
        // sslmode are upgraded to `require` - see `connect_options`). Built by
        // deserialization because the secret-wrapped `database_url` has no
        // public literal constructor (the production path is always serde +
        // expand-vars). `feed_replay_horizon_secs` has no working default and is
        // required, so it is set here too: without it this harness would build
        // every pg test on a config `validate` rejects.
        let cfg: TimescaleDbPluginConfig = serde_json::from_str(&format!(
            r#"{{ "database_url": "postgres://user:pass@127.0.0.1:{port}/app?sslmode=disable",
                  "statement_timeout_secs": {statement_timeout_secs},
                  "feed_replay_horizon_secs": 3600,
                  "feed_acceptance_slack_secs": {feed_acceptance_slack_secs},
                  "pool_size_min": {pool_size_min}, "pool_size_max": {pool_size_max} }}"#
        ))
        .expect("valid test config json");

        // A server that has announced itself is not the same as one accepting a
        // TCP connection this instant under load, so a connect gets its own
        // short budget before the container is written off. Ten seconds per
        // container, three containers: the same 30 s ceiling the flat loop had,
        // spent where it can actually help.
        let mut pool = None;
        let mut connect_err = None;
        for _ in 0..CONNECT_ATTEMPTS {
            match build_pool(&cfg).await {
                Ok(p) => {
                    pool = Some(p);
                    break;
                }
                Err(e) => {
                    connect_err = Some(e);
                    tokio::time::sleep(CONNECT_INTERVAL).await;
                }
            }
        }
        match pool {
            Some(pool) => {
                brought_up = Some((container, pool, cfg));
                break;
            }
            None if last => {
                return Err(anyhow::anyhow!(
                    "pool connect failed on every one of {CONTAINER_ATTEMPTS} containers, \
                     each given {CONNECT_ATTEMPTS} attempts over {:?}: {connect_err:?}",
                    CONNECT_INTERVAL * CONNECT_ATTEMPTS,
                ));
            }
            None => {
                let detail = connect_err
                    .as_ref()
                    .map_or_else(|| "no error recorded".to_owned(), ToString::to_string);
                eprintln!(
                    "timescaledb container published a port that would not serve a pool \
                     (attempt {attempt}/{CONTAINER_ATTEMPTS}): {detail}; starting another"
                );
                drop(container);
                tokio::time::sleep(CONTAINER_RETRY_BACKOFF).await;
            }
        }
    }
    let (container, pool, cfg) = brought_up
        .ok_or_else(|| anyhow::anyhow!("container bring-up loop ended without a container"))?;

    MIGRATOR.run(&pool).await?;
    apply_post_migration_setup(&pool, &cfg).await?;
    settle_and_remove_rollup_policies(&pool).await?;
    Ok(TsHarness {
        pool,
        cfg,
        _container: container,
    })
}

/// How many times [`await_setting`] reads a server setting back, and how long
/// it waits between reads - 5 seconds in total.
///
/// `pg_reload_conf()` signals the postmaster and returns before every backend
/// has re-read the file, so a caller that asserted straight after it would be
/// asserting against the value the connection still held. Measured on
/// `timescale/timescaledb:2.29.2-pg18`, a `sighup` setting flipped by
/// `ALTER SYSTEM` reads back changed on the very next statement, on a session
/// that was already open; the budget is what turns a slower box into a failure
/// rather than a hang.
const SETTING_ATTEMPTS: u32 = 50;
const SETTING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Block until `current_setting(setting)` reads `expected` on `pool`.
///
/// # Panics
///
/// If it still does not after [`SETTING_ATTEMPTS`] reads, naming the setting,
/// what was wanted and what was last read. A caller that carried on regardless
/// would be asserting against a server it has not in fact reconfigured, which
/// surfaces as a flaky *failure* rather than a false pass: `startup_durability_pg`
/// would get the pool its `expect_err` refuses to accept.
pub async fn await_setting(pool: &PgPool, setting: &str, expected: &str) {
    let mut last = String::new();
    for _ in 0..SETTING_ATTEMPTS {
        last = sqlx::query_scalar("SELECT current_setting($1)")
            .bind(setting)
            .fetch_one(pool)
            .await
            .expect("read the setting back");
        if last == expected {
            return;
        }
        tokio::time::sleep(SETTING_INTERVAL).await;
    }
    panic!(
        "{setting} still reads {last} after {:?}, wanted {expected}",
        SETTING_INTERVAL * SETTING_ATTEMPTS
    );
}

/// Wait for the two refresh policies' first run, which `TimescaleDB` starts
/// within seconds of creating them, then delete them. A background refresh
/// racing a test would make "stale until refreshed" assertions flaky; tests
/// refresh explicitly through [`refresh_rollup`] instead. A test that
/// re-applies setup (which re-creates the policies) must call this again
/// afterwards, once it is done asserting on the policies, so they cannot race
/// the sweep.
pub async fn settle_and_remove_rollup_policies(pool: &PgPool) -> anyhow::Result<()> {
    for _ in 0..60 {
        let ran: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM timescaledb_information.jobs j \
             JOIN timescaledb_information.job_stats js ON js.job_id = j.job_id \
             WHERE j.proc_name = 'policy_refresh_continuous_aggregate' AND js.total_runs >= 1",
        )
        .fetch_one(pool)
        .await?;
        if ran >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    sqlx::query(
        "SELECT delete_job(job_id) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_refresh_continuous_aggregate'",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Materialise every invalidated range of the rollup now. Autocommit only:
/// `refresh_continuous_aggregate` refuses a transaction block.
pub async fn refresh_rollup(pool: &PgPool) {
    sqlx::query("CALL refresh_continuous_aggregate('usage_rollup_1h', NULL, NULL)")
        .execute(pool)
        .await
        .expect("refresh the rollup");
}

/// The `xact_id` the ledger stamped on the stored entry `id`.
///
/// Read as text and parsed, because `xid8` has no `sqlx` `Decode`
/// implementation — the same reason `UsageRecordRow::xact_id` is a `String`.
/// The parse is not cosmetic: it is what makes `<` an order over transaction
/// ids rather than over their digit strings, which disagree as soon as two
/// ids differ in length.
///
/// # Panics
///
/// If no row carries `id`, or the stored value does not parse as a `u64`. The
/// first means the write under test did not happen; the second means the
/// column is no longer an `xid8`.
pub async fn xact_id_of(pool: &PgPool, id: Uuid) -> u64 {
    let rendered: String =
        sqlx::query_scalar("SELECT xact_id::text FROM usage_records WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("the entry must be stored");
    rendered
        .parse()
        .unwrap_or_else(|e| panic!("xact_id {rendered} must render as a u64: {e}"))
}

/// Build a fresh metric inventory over `pool`.
///
/// Private: [`record_store`] is its only caller, and a `pub` helper here is a
/// helper the `dead_code` allowance would hide if that caller went away.
///
/// The stores now take an `Arc<Metrics>`; tests only need a live handle, not to
/// assert on it, so each call mints its own inventory against the global meter
/// provider (recording is a no-op without an exporter installed).
#[must_use]
fn metrics(pool: &PgPool) -> Arc<Metrics> {
    Arc::new(Metrics::new(pool.clone()))
}

/// Convenience builder for a [`PgRecordStore`] with its own metric handle,
/// over the harness's own pool and acceptance slack.
///
/// It takes the whole [`TsHarness`] rather than its pool, because the store now
/// reads one setting off the config too: a builder taking the pool alone would
/// have to pick an acceptance slack of its own, and a suite that set one on the
/// harness would silently not get it.
#[must_use]
pub fn record_store(h: &TsHarness) -> PgRecordStore {
    PgRecordStore::new(
        h.pool.clone(),
        metrics(&h.pool),
        CancellationToken::new(),
        h.cfg.feed_acceptance_slack_secs,
    )
}

/// Bring up a migrated `TimescaleDB` and return the SPI implementation over it.
///
/// The DESIGN section 3.3 contract suite takes a `&dyn
/// UsageCollectorPluginV1`, and [`StorageAdapter`] is the crate's only
/// implementation of it, so this is the whole of the wiring: the same
/// container [`bring_up`] starts, the same [`PgRecordStore`] every other
/// caller of [`record_store`] drives, behind the adapter the gear registers
/// in `ClientHub`.
///
/// Nothing in the harness drops data: chunks are dropped only by the retention
/// sweep, which a test runs explicitly.
///
/// The returned [`TsHarness`] owns the container: hold it for the length of
/// the test, or the database goes away with it. The suite writes entries and
/// never removes them, but each call starts a container of its own, so a run
/// never meets a previous run's rows and the fixtures' keying assumptions
/// never come into it.
///
/// # Panics
///
/// If the container or the migration fails; there is no test to run without
/// a backend.
pub async fn start_backend() -> (TsHarness, StorageAdapter) {
    let harness = bring_up()
        .await
        .expect("contract suite needs a migrated TimescaleDB container");
    let store: Arc<dyn RecordStore> = Arc::new(record_store(&harness));
    (harness, StorageAdapter::new(store))
}

// ---------------------------------------------------------------------------
// Ledger-entry fixtures
//
// Authored here rather than in each suite because four of the five share them,
// which is the same sharing the file-header `dead_code` allowance is earned by.
// What Task 14 declined to do was *port* the retired builders; these are built
// against the current `UsageRecord` and every one of them is run.
//
// The one decision a fixture cannot avoid is the covered period, because both
// bounds are inputs to the derived identity
// (`cpt-cf-usage-collector-adr-record-identity-derivation`). It is taken once,
// here, as [`FIXTURE_WINDOW_START`] / [`FIXTURE_WINDOW_END`], so a suite that
// needs a *different* period says so at the call site instead of every suite
// picking one.
// ---------------------------------------------------------------------------

/// A valid meter type id: the reserved base plus one derivation segment,
/// `~`-terminated, which is what `MeterTypeId::new` validates.
pub const VCPU_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";

/// A second meter, for the assertions whose subject is that a scope is per
/// `(tenant_id, gts_type_id)` rather than per tenant.
pub const GB_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.storage._.gb_hours.v1~";

/// Inclusive start of the covered period every fixture carries by default:
/// `2023-11-14T22:13:20Z`.
pub const FIXTURE_WINDOW_START_UNIX: i64 = 1_700_000_000;

/// Exclusive end of that period, one hour later. Distinct from the start, so a
/// fixture is a period rather than a point event — a point event is a case the
/// suites ask for explicitly (`window_start == window_end`) rather than the
/// shape everything else accidentally inherits.
pub const FIXTURE_WINDOW_END_UNIX: i64 = 1_700_003_600;

/// The parsed [`FIXTURE_WINDOW_START_UNIX`].
///
/// # Panics
///
/// Never: the constant is a valid Unix instant.
#[must_use]
pub fn fixture_window_start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(FIXTURE_WINDOW_START_UNIX).expect("valid instant")
}

/// The parsed [`FIXTURE_WINDOW_END_UNIX`].
///
/// # Panics
///
/// Never: the constant is a valid Unix instant.
#[must_use]
pub fn fixture_window_end() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(FIXTURE_WINDOW_END_UNIX).expect("valid instant")
}

/// A meter id from its wire string.
///
/// # Panics
///
/// If `id` is not a valid meter type id; every caller here passes a constant.
#[must_use]
pub fn meter(id: &str) -> MeterTypeId {
    MeterTypeId::new(id).expect("valid meter type id")
}

/// An ordinary measurement over an explicit covered period, with the derived
/// identity the Ingestion Gateway would stamp on it.
///
/// The `id` is [`derive_usage_record_id`] over the same six inputs the ledger's
/// `usage_records_dedup_uniq` is built over — this is the gateway's job, not the
/// plugin's, so deriving it here is standing in for the gateway rather than
/// asking the code under test to check itself.
///
/// The sixth input is the entry type, which is [`EntryType::Record`] here
/// because this builds a measurement: the invalidation pair is `None` below,
/// and [`withdrawal_of`] is what builds the other kind.
///
/// Every field that is **not** one of those six (quantity, attribution,
/// metadata, origin) can be overwritten with a struct update afterwards without
/// invalidating the identity. The five caller-chosen ones appear in this
/// signature for exactly that reason; setting the invalidation pair afterwards
/// does move the entry's identity, which is what [`rederive`] is for.
///
/// # Panics
///
/// If `idem` is not a valid idempotency key, or the fixed resource reference
/// fails to validate; both are constants here.
#[must_use]
pub fn entry_over(
    meter_id: &MeterTypeId,
    tenant: Uuid,
    idem: &str,
    value: Decimal,
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
) -> UsageRecord {
    let idempotency_key = IdempotencyKey::new(idem).expect("valid idempotency key");
    UsageRecord {
        id: derive_usage_record_id(
            tenant,
            meter_id,
            &idempotency_key,
            window_start,
            window_end,
            EntryType::Record,
        ),
        gts_type_id: meter_id.clone(),
        tenant_id: tenant,
        resource_ref: ResourceRef::new("res-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: UsageQuantity::try_from(value).expect("fixture quantity"),
        idempotency_key,
        accepted_at: fixture_window_end(),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start,
        window_end,
    }
}

/// [`entry_over`] at the default covered period.
#[must_use]
pub fn entry(meter_id: &MeterTypeId, tenant: Uuid, idem: &str, value: Decimal) -> UsageRecord {
    entry_over(
        meter_id,
        tenant,
        idem,
        value,
        fixture_window_start(),
        fixture_window_end(),
    )
}

/// A faithful withdrawal of `target`: a copy of the entry it withdraws, plus the
/// invalidation pair, under the target's own idempotency key.
///
/// The published schema's own phrase for this shape is *"An invalidation is a
/// faithful copy of its target"* (`usage-collector-v1.yaml`), and every field
/// copied below is copied for a reason rather than for tidiness:
///
/// * **The quantity** — an invalidation echoes what it withdraws rather than
///   negating it (`cpt-cf-usage-collector-adr-append-only-invalidation`), which
///   is why netting the two would now double-count.
/// * **The covered period** — two of the six dedup-identity inputs, so a
///   withdrawal carrying a different period is a different identity and no
///   longer collides with another withdrawal of the same target.
/// * **The idempotency key** — a third. A withdrawal repeats its target's, so
///   the pair departs in the entry type alone.
/// * **The attribution, metadata and origin** — so the only fields separating
///   the pair are `invalidates` and `reason_code`. A withdrawal that quietly
///   differed in, say, `resource_type` would let a `$filter` test look like it
///   discriminated when it had only found an asymmetry the fixture put there.
///
/// The entry type is the one input to the derivation the two do not share, and
/// is therefore the whole reason their identifiers differ
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`). Two withdrawals
/// of one target agree on all six and derive one identifier. There is no
/// parameter for the key: it is the target's, so `entry_over`'s `idem` argument
/// only ever seeds a value this function immediately overwrites.
///
/// # Panics
///
/// If the fixed reason code fails to validate.
#[must_use]
pub fn withdrawal_of(target: &UsageRecord) -> UsageRecord {
    rederive(UsageRecord {
        idempotency_key: target.idempotency_key.clone(),
        invalidation: Some(Invalidation {
            target: target.id,
            reason: ReasonCode::new("duplicate_submission").expect("valid reason code"),
        }),
        resource_ref: target.resource_ref.clone(),
        subject_ref: target.subject_ref.clone(),
        metadata: target.metadata.clone(),
        origin: target.origin,
        ..entry_over(
            &target.gts_type_id,
            target.tenant_id,
            "withdrawal-of-placeholder-key",
            target.quantity.as_decimal(),
            target.window_start,
            target.window_end,
        )
    })
}

/// A withdrawal of `target` carrying `reason` instead of [`withdrawal_of`]'s
/// fixed `"duplicate_submission"`.
///
/// Every withdrawal of one target carries the target's own idempotency key and
/// `entry_type = invalidation`, so all six identity inputs agree
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`) and two
/// withdrawals of one target built through [`withdrawal_of`] alone would be
/// byte-for-byte identical and absorbed as an idempotent replay rather than
/// decided against each other. `reason_code` is the one field that survives
/// that exclusion, so a test building a *second* withdrawal of one target
/// uses this to keep the two apart.
///
/// # Panics
///
/// If `reason` fails to validate.
#[must_use]
pub fn withdrawal_of_with_reason(target: &UsageRecord, reason: &str) -> UsageRecord {
    UsageRecord {
        invalidation: Some(Invalidation {
            target: target.id,
            reason: ReasonCode::new(reason).expect("valid reason code"),
        }),
        ..withdrawal_of(target)
    }
}

/// Restamp `record`'s derived identity after one of the six dedup-identity
/// inputs was changed by a struct update.
///
/// [`entry_over`] takes five of the six as parameters precisely so this is
/// rarely needed — every field a caller usually overwrites afterwards
/// (quantity, attribution, metadata, origin) is outside the derivation. Two
/// cases are left. Setting the invalidation pair changes the sixth input, the
/// entry type, which is why [`withdrawal_of`] ends here. And a test that starts
/// from [`withdrawal_of`] and then moves the entry into a different scope
/// changes `tenant_id` or `gts_type_id`, which *are* inputs. Leaving the
/// stamped id alone in either case would store a row whose id no emitter could
/// reproduce, and the ledger's `id` and its dedup UNIQUE would disagree about
/// what the entry is.
///
/// The entry type is read off the record rather than passed in, so it cannot be
/// stamped as anything other than what the record's own invalidation pair says
/// it is.
///
/// # Panics
///
/// Never: the record already carries a validated key and meter.
#[must_use]
pub fn rederive(record: UsageRecord) -> UsageRecord {
    UsageRecord {
        id: derive_usage_record_id(
            record.tenant_id,
            &record.gts_type_id,
            &record.idempotency_key,
            record.window_start,
            record.window_end,
            record.entry_type(),
        ),
        ..record
    }
}

/// The compiled PDP scope a read is intersected with: every entry of `tenant`.
///
/// `get` takes the scope as its whole filter, so a test that wants a point
/// lookup to succeed has to hand it one the row satisfies. This is the narrowest
/// honest one — a real grant is a disjunction over the tenants a principal
/// holds, and a single-tenant grant is one arm of it.
#[must_use]
pub fn tenant_scope(tenant: Uuid) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(tenant))),
    )
}

/// Insert one `usage_records` row binding every column directly, so a test can
/// write a shape the SPI cannot express.
///
/// Every column is defaulted to a well-formed ordinary measurement, and the
/// columns `usage_records_invalidation_pairing` spans are the settable ones,
/// because they are the whole point: the SPI builds an entry's kind and its
/// withdrawal pair from one `Option<Invalidation>` (`UsageRecord::entry_type`,
/// `mapper::invalidation_to_row`), so a row whose declared kind disagrees with
/// its pair is unreachable through `RecordStore::create` and reachable only
/// here.
///
/// It binds `entry_type` explicitly, which is what makes it an escape hatch
/// rather than a second `create`: the column is written from the dispatched
/// entry's declared kind, so a raw insert is the only writer that can declare
/// one thing and store another.
#[must_use]
pub fn insert_raw_entry(pool: &PgPool) -> RawEntry<'_> {
    RawEntry {
        pool,
        id: Uuid::new_v4(),
        entry_type: "record",
        invalidates: None,
        reason_code: None,
    }
}

/// The tenant every [`insert_raw_entry`] row is written under.
///
/// The max UUID, because every other tenant in these suites is a small
/// hand-picked integer: a raw row can then never be mistaken for one an
/// SPI-driven test wrote, whatever values that test picks later.
const RAW_ENTRY_TENANT: Uuid = Uuid::max();

/// The builder [`insert_raw_entry`] returns. See its doc.
pub struct RawEntry<'a> {
    pool: &'a PgPool,
    id: Uuid,
    entry_type: &'a str,
    invalidates: Option<Uuid>,
    reason_code: Option<&'a str>,
}

impl<'a> RawEntry<'a> {
    /// Declare the entry's kind, as the `usage_entry_type` label to store.
    /// Taken as a string rather than as [`EntryType`] so a test can write a
    /// label the enum does not carry.
    #[must_use]
    pub const fn entry_type(mut self, kind: &'a str) -> Self {
        self.entry_type = kind;
        self
    }

    /// Set (or clear) the entry this one withdraws.
    #[must_use]
    pub const fn invalidates(mut self, target: Option<Uuid>) -> Self {
        self.invalidates = target;
        self
    }

    /// Set (or clear) the withdrawal reason.
    #[must_use]
    pub const fn reason_code(mut self, reason: Option<&'a str>) -> Self {
        self.reason_code = reason;
        self
    }

    /// Run the insert.
    ///
    /// Returns the raw `sqlx` error on refusal, so a caller can assert on the
    /// constraint name `PostgreSQL` reports.
    ///
    /// The type key is bound as a literal rather than resolved through
    /// `usage_type_key`: nothing references that table, so any integer
    /// partitions the row correctly for a test that never reads it back by
    /// type.
    ///
    /// # Errors
    ///
    /// Whatever `PostgreSQL` refuses the row with.
    pub async fn execute(self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO usage_records (id, tenant_id, gts_type_id, type_key, quantity, \
                 window_start, window_end, resource_id, resource_type, subject_id, \
                 subject_type, idempotency_key, invalidates, reason_code, origin, \
                 entry_type, accepted_at, metadata) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL, NULL, $10, $11, $12, 'live', \
                 $13::usage_entry_type, $14, '{}'::jsonb)",
        )
        .bind(self.id)
        .bind(RAW_ENTRY_TENANT)
        .bind(VCPU_METER)
        .bind(1_i32)
        .bind(Decimal::ONE)
        .bind(fixture_window_start())
        .bind(fixture_window_end())
        .bind("raw-resource-id")
        .bind("raw-resource-type")
        .bind(self.id.to_string())
        .bind(self.invalidates)
        .bind(self.reason_code)
        .bind(self.entry_type)
        .bind(fixture_window_end())
        .execute(self.pool)
        .await
        .map(|_| ())
    }
}
