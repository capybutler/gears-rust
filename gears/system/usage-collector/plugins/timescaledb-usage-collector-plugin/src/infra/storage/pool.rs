// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (hypertable
// time-series, `time_bucket` aggregation, keyset pagination — see DESIGN.md). Tenant
// isolation is enforced by hand via parameterized `tenant_id` predicates and an
// allowlisted-identifier query builder (DESIGN.md §Injection-Safe Query Translation),
// not SecureConn/AccessScope.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::str::FromStr;
use std::time::Duration;

use secrecy::ExposeSecret;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgConnection, PgPool};

use crate::config::TimescaleDbPluginConfig;
use crate::infra::storage::rollup_maintenance::apply_rollup_policies;

/// Embedded schema migrations (`migrations/` at crate root).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Parse the DSN into connection options with TLS enforced by default.
///
/// TLS is the plugin's one stated security obligation (DESIGN §3.5 / §Security),
/// so it is enforced here rather than left to operator DSN convention: sqlx's
/// default is `prefer`, which silently falls back to plaintext. The silent
/// modes — an unspecified `sslmode`, `prefer`, or `allow` — are upgraded to
/// `require` so credentials and usage data are never sent in cleartext by
/// omission. A stronger operator choice (`verify-ca` / `verify-full`) is
/// preserved.
///
/// An explicit `sslmode=disable` is honored as a deliberate, auditable opt-out
/// (trusted networks / local dev / the integration test container, which serves
/// no TLS). This still closes the defect — the *silent* plaintext fallback —
/// while leaving a documented escape hatch.
///
/// # Errors
/// Returns `sqlx::Error` if the DSN cannot be parsed.
fn connect_options(database_url: &str) -> Result<PgConnectOptions, sqlx::Error> {
    let opts = PgConnectOptions::from_str(database_url)?;
    let resolved = match opts.get_ssl_mode() {
        // Silent fallback modes (incl. the unspecified default `prefer`): upgrade.
        PgSslMode::Allow | PgSslMode::Prefer => opts.ssl_mode(PgSslMode::Require),
        // Explicit `disable` is a deliberate opt-out; `require` and the verifying
        // modes already meet the obligation. All kept as-is.
        PgSslMode::Disable | PgSslMode::Require | PgSslMode::VerifyCa | PgSslMode::VerifyFull => {
            opts
        }
    };
    if is_plaintext(resolved.get_ssl_mode()) {
        // A documented-but-silent plaintext opt-out is invisible to operators, so
        // surface it in logs/alerting at startup. Emitted once per pool build
        // (this fn is called once from `build_pool`), not once per connection.
        tracing::warn!(
            "connecting to TimescaleDB with sslmode=disable: credentials and usage \
             data are sent in cleartext. This is a deliberate opt-out; set \
             sslmode=require (or verify-ca/verify-full) for encrypted transport."
        );
    }
    Ok(resolved)
}

/// Whether the enforced SSL mode is a plaintext opt-out (data sent in cleartext).
/// Only an explicit `disable` reaches here as plaintext, since [`connect_options`]
/// upgrades the silent fallbacks (`prefer`/`allow`) to `require` first.
fn is_plaintext(mode: PgSslMode) -> bool {
    matches!(mode, PgSslMode::Disable)
}

/// Fixed upper bound on how long a request-path statement waits on a contended
/// lock. The wait it is *sized* for is the speculative tuple an
/// `INSERT ... ON CONFLICT ... DO NOTHING` meets when a not-yet-committed
/// duplicate of the same dedup 6-tuple is in flight. The wait then fails fast
/// (`55P03 lock_not_available`) instead of blocking on — and pinning — a pooled
/// connection.
///
/// **The per-scope counter is gone.** This plugin took a second lock
/// on every write until the per-scope counter row it claimed from was retired, so
/// a write contended with unrelated traffic on a busy tenant or meter. It no
/// longer does: on the dedup tuple a write contends only with another writer of
/// the very same entry.
///
/// The bound is not sized for it, but it covers every other wait an ingest
/// statement can meet. Two are worth naming because neither is contention with
/// another writer of the same entry, which is what the sizing is about:
///
/// * [`super::type_key::TypeKeyCache::resolve`] assigns a type's partition
///   key with its own `INSERT ... ON CONFLICT`, in autocommit before the write
///   transaction opens, so two first writes of one *type* meet on that tuple. It
///   is once per type per process rather than once per write, and it is held for
///   the statement rather than to a commit.
/// * An insert routed to a chunk the retention sweep is dropping waits behind
///   that sweep's `ACCESS EXCLUSIVE` chunk lock, which is held to the sweep's
///   own commit under a `lock_timeout` of its own (this plugin's DESIGN §3.6
///   `cpt-cf-uc-plugin-seq-retention-sweep`). The sweep reasons about this
///   contention from its side, where a timed-out wait keeps the chunk; from
///   ingest's side it arrives as the same `55P03` as any other lock wait.
///
/// `55P03` is classified transient ([`super::error`]) precisely because a wait
/// that times out here is an ordinary contention outcome rather than a defect,
/// whichever lock it was on, so a timed-out batch is retried rather
/// than returned as a non-retryable failure. That classification is what makes
/// the list above a reading aid rather than a premise: nothing depends on it
/// being complete.
const LOCK_TIMEOUT: &str = "5s";

/// Session GUCs applied to every request-path pool connection at connect time:
/// `statement_timeout` (config-driven) bounds how long a statement may run,
/// `transaction_timeout` (config-driven) how long a whole transaction may stay
/// open, and `lock_timeout` (fixed at [`LOCK_TIMEOUT`]) how long a statement
/// waits on a contended lock, so a wedged backend cannot pin pool connections
/// indefinitely and exhaust the pool. Applied as `-c name=value` startup
/// parameters so the bounds hold from the connection's first query, with no
/// extra round-trip.
///
/// `transaction_timeout` bounds the plugin's own request-path and retention-drop
/// transactions, so neither can hold the feed's settled horizon back
/// indefinitely (`docs/DESIGN.md` §4.1 item 2). It does not bound the horizon
/// itself: the horizon is cluster-wide, bounded by the longest-running write
/// transaction in the `PostgreSQL` instance, and item 2's table leaves the
/// rollup refresh "not bounded by configuration" and anything else in the
/// instance "not bounded by the plugin". A statement bound alone would not give
/// even this much: a transaction that opened and then stalled between statements
/// holds the horizon back with every one of its statements inside the bound. The
/// retention sweep's detached connection is `pool.acquire().await?.detach()`
/// ([`super::retention_sweep`]) — the same physical connection with the same
/// startup parameters — so it carries this bound already and needs no `SET` of
/// its own.
fn connection_gucs(
    statement_timeout_secs: u64,
    transaction_timeout_secs: u64,
) -> [(&'static str, String); 3] {
    [
        ("statement_timeout", format!("{statement_timeout_secs}s")),
        (
            "transaction_timeout",
            format!("{transaction_timeout_secs}s"),
        ),
        ("lock_timeout", LOCK_TIMEOUT.to_owned()),
    ]
}

/// Build the request-path connect options: DSN parsing + TLS enforcement
/// ([`connect_options`]) plus the bounding session GUCs ([`connection_gucs`]).
///
/// # Errors
/// Returns `sqlx::Error` if the DSN cannot be parsed.
fn pool_connect_options(
    database_url: &str,
    statement_timeout_secs: u64,
    transaction_timeout_secs: u64,
) -> Result<PgConnectOptions, sqlx::Error> {
    Ok(connect_options(database_url)?.options(connection_gucs(
        statement_timeout_secs,
        transaction_timeout_secs,
    )))
}

/// Server-wide settings the plugin refuses to start without.
///
/// `docs/DESIGN.md` §3.5 states both the rule and the reason it cannot be met
/// per transaction: "Unlike `synchronous_commit`, `fsync` and
/// `full_page_writes` are server-wide and cannot be forced per transaction, and
/// either one off can lose a committed write on a crash."
///
/// The `TimescaleDB` extension check that the same provisioning steps call for
/// (`inst-pool-extension` in `docs/features/registration-schema-provisioning.md`,
/// alongside `inst-pool-fsync` and `inst-pool-full-page-writes`) is met by the
/// migration's `CREATE EXTENSION IF NOT EXISTS timescaledb`, which fails when
/// the extension is unavailable. No second check is added for it.
const REQUIRED_DURABILITY_SETTINGS: [&str; 2] = ["fsync", "full_page_writes"];

/// Verify every setting in [`REQUIRED_DURABILITY_SETTINGS`] reads `on`.
///
/// Called from [`build_pool`] before it returns, so a failure leaves the plugin
/// unregistered rather than running against a server that can lose an
/// acknowledged write (`inst-pool-return`).
///
/// # Errors
/// Returns `sqlx::Error` if a setting cannot be read, or
/// `sqlx::Error::Configuration` naming the setting and its value if it is not
/// `on`.
async fn verify_durability_settings(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    for setting in REQUIRED_DURABILITY_SETTINGS {
        let value: String = sqlx::query_scalar("SELECT current_setting($1)")
            .bind(setting)
            .fetch_one(&mut *conn)
            .await?;
        if value != "on" {
            return Err(sqlx::Error::Configuration(
                format!(
                    "TimescaleDB server reports {setting} = {value}; this plugin refuses to start \
                     against a server that can lose an acknowledged write. The setting is \
                     server-wide and cannot be forced per transaction, so it is the operator's to \
                     fix on the server"
                )
                .into(),
            ));
        }
    }
    Ok(())
}

/// Build the connection pool with TLS enforced (`sslmode >= require`, see
/// `connect_options`) and every request-path connection bounded by
/// `statement_timeout` + `transaction_timeout` + `lock_timeout` (see
/// `connection_gucs`), then refuse the server outright unless every setting
/// in `REQUIRED_DURABILITY_SETTINGS` reads `on`.
///
/// # Errors
/// Returns `sqlx::Error` if the DSN is malformed, the pool cannot connect
/// within the timeout, or a durability setting is not `on`.
pub async fn build_pool(cfg: &TimescaleDbPluginConfig) -> Result<PgPool, sqlx::Error> {
    // Unwrap the secret DSN only here, at the connection boundary: keep it behind
    // `secrecy`'s opaque-debug/zeroize guarantees and expose the bytes just long
    // enough for sqlx to parse them into `PgConnectOptions`.
    let dsn = cfg.database_url.clone_into_secret_string();
    let pool = PgPoolOptions::new()
        .min_connections(cfg.pool_size_min)
        .max_connections(cfg.pool_size_max)
        .acquire_timeout(Duration::from_secs(cfg.connection_timeout_secs))
        .connect_with(pool_connect_options(
            dsn.expose_secret(),
            cfg.statement_timeout_secs,
            cfg.transaction_timeout_secs,
        )?)
        .await?;
    // On one acquired connection, and before the pool is handed out: the caller
    // never sees a pool built against a server that fails the check.
    let mut conn = pool.acquire().await?;
    verify_durability_settings(&mut conn).await?;
    drop(conn);
    Ok(pool)
}

/// Fixed advisory-lock key namespacing the plugin's post-migration setup.
/// Arbitrary but stable; the plugin owns its database, so a collision with an
/// unrelated advisory lock is not a concern. (`0x7563_7462` == ASCII `"uctb"`.)
const INIT_ADVISORY_LOCK_KEY: i64 = 0x7563_7462;

/// Acquire the init advisory lock on `lock_conn`, serializing concurrent replica
/// init so only one applies the post-migration policy registration at a time.
///
/// `pg_advisory_lock` has no timeout of its own, but `lock_conn` is drawn from
/// [`build_pool`], which sets `statement_timeout` on every connection (see
/// [`connection_gucs`]). That connection-level bound aborts the blocking
/// `SELECT pg_advisory_lock(...)` with `57014` (query-canceled) if a wedged peer
/// holds the lock, so init fails fast and the orchestrator can retry instead of
/// stalling indefinitely. No per-lock `statement_timeout` override is applied, so
/// nothing is left set on the connection when it returns to the pool.
///
/// # Errors
/// Returns `sqlx::Error` if the lock cannot be acquired (including a
/// `statement_timeout` abort while a peer holds it).
async fn acquire_init_lock(lock_conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    if let Err(e) = sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(INIT_ADVISORY_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await
    {
        tracing::error!(
            error = %e,
            "failed to acquire init advisory lock; a peer replica may be holding it, \
             so init fails fast to be retried"
        );
        return Err(e);
    }
    Ok(())
}

/// Run the post-migration partitioning and rollup-policy setup under a
/// database advisory lock so concurrently-initializing replicas serialize
/// here.
///
/// A session-level `pg_advisory_lock` held on a dedicated connection for the
/// whole section lets only one replica apply at a time; the rest block until it
/// releases. (Schema migrations themselves are already serialized by sqlx's own
/// migration lock; this covers the setup that sqlx does not.)
///
/// The setup statements keep running in autocommit on the pool — deliberately
/// *not* wrapped in an explicit transaction, since `TimescaleDB` policy
/// functions are happiest in autocommit. The lock is released on every return
/// path; if the holding process dies, Postgres releases it when the session
/// ends.
///
/// The wait to *acquire* the lock is bounded by the connection-level
/// `statement_timeout` (see `acquire_init_lock`, named in plain backticks
/// because it is private and this item is public) so a wedged peer cannot stall
/// init forever.
///
/// # Errors
/// Returns `sqlx::Error` if the lock cannot be acquired or a setup statement
/// fails.
pub async fn apply_post_migration_setup(
    pool: &PgPool,
    cfg: &TimescaleDbPluginConfig,
) -> Result<(), sqlx::Error> {
    let mut lock_conn = pool.acquire().await?;
    acquire_init_lock(&mut lock_conn).await?;

    let result = async {
        apply_partitioning(pool, cfg.chunk_time_interval_secs, cfg.type_key_slice_width).await?;
        apply_rollup_policies(pool, cfg).await
    }
    .await;

    // Release on every path (including the error path) so a failing replica
    // never wedges the others. If the unlock itself fails the session is likely
    // already broken, in which case Postgres frees the lock when it ends.
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(INIT_ADVISORY_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await
    {
        tracing::warn!(
            error = %e,
            "failed to release init advisory lock; it frees when the session ends"
        );
    }

    result
}

/// Remove any table-wide retention policy and apply the configured chunk
/// intervals. Idempotent: a restart with changed values applies them.
///
/// The policy removal matters for a database an earlier build initialized: a
/// table-wide `policy_retention` drops every type at one horizon, underneath the
/// per-type retention sweep.
///
/// Both intervals apply to chunks created afterwards; existing chunks keep their
/// ranges. `dimension_name` is required on both calls — with two dimensions,
/// `TimescaleDB` refuses an unnamed interval change as ambiguous.
///
/// # Errors
/// Returns `sqlx::Error` if any statement fails.
pub async fn apply_partitioning(
    pool: &PgPool,
    chunk_time_interval_secs: u64,
    type_key_slice_width: u32,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT remove_retention_policy('usage_records', if_exists => TRUE)")
        .execute(pool)
        .await?;
    let secs = i64::try_from(chunk_time_interval_secs).unwrap_or(i64::MAX);
    sqlx::query(
        "SELECT set_chunk_time_interval('usage_records', \
         make_interval(secs => $1::double precision), dimension_name => 'window_end')",
    )
    .bind(secs)
    .execute(pool)
    .await?;
    sqlx::query(
        "SELECT set_chunk_time_interval('usage_records', $1::bigint, \
         dimension_name => 'type_key')",
    )
    .bind(i64::from(type_key_slice_width))
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "pool_tests.rs"]
mod pool_tests;
