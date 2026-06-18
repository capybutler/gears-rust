# TimescaleDB Plugin — Lifecycle Cancellation (REVIEW issues 1 & 2)

- **Date:** 2026-06-22
- **Scope:** `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`
- **Source:** `REVIEW.md` findings #1 and #2 (both `TOOLKIT-LIFE-001`, MED).

## Goal

Make the plugin's startup I/O interruptible at shutdown, and stop its detached
background gauge-refresh task from leaking a pooled connection on teardown. Both
are cooperative-cancellation gaps against the toolkit's `CancellationToken`
lifecycle.

## Background

- `GearCtx::cancellation_token()` returns a `&CancellationToken` (a child of the
  runtime root token) that fires when the runtime begins shutdown.
- This plugin only implements `Gear::init` — there is no `stop`/`RunnableCapability`
  hook, so there is nowhere to await outstanding background tasks. Cooperative
  cancellation via the token is therefore the correct mechanism: when the token
  fires, in-flight futures are dropped and their connections returned to the pool.
- The crate's OTel observable-gauge callbacks are synchronous and documented as
  doing **no DB I/O** (`src/infra/metrics.rs`). That rules out the review's
  "ObservableGauge callback" alternative for issue 2 — an async `count(*)` cannot
  run inside a sync collection callback without blocking the meter cycle.

## Issue 1 — `init` cannot be interrupted during startup

**Location:** `src/gear.rs:42`

**Problem:** `init` runs unbounded DB I/O — `build_pool` → `Metrics::new` →
`MIGRATOR.run` → `apply_retention_policy` → `apply_dedup_cleanup_job` — without
observing the cancellation token. If shutdown begins mid-startup (e.g. a slow or
unreachable DB), init blocks until each call's own timeout instead of aborting.

**Fix:** Race the connect/migrate/retention sequence against
`ctx.cancellation_token().cancelled()` with `tokio::select!`. On cancel, return
`Err(anyhow!("init cancelled during shutdown"))` so startup aborts cleanly rather
than reporting success on a half-wired plugin.

Scope is the connect/migrate/retention sequence only (as the review names it);
the registry/ClientHub registration below it is left unchanged. Because
`Metrics::new` needs the pool and the migration-failure counter must still fire
on a migration error, the whole sequence moves into the raced `async` block and
yields `(pool, metrics)` back out:

```rust
let cancel = ctx.cancellation_token().clone();
let (pool, metrics) = tokio::select! {
    biased;
    () = cancel.cancelled() => {
        return Err(anyhow::anyhow!("init cancelled during shutdown"));
    }
    res = async {
        let pool = build_pool(&cfg).await?;
        let metrics = Arc::new(Metrics::new(pool.clone()));
        if let Err(e) = MIGRATOR.run(&pool).await {
            metrics.inc_migration_failure();
            return Err::<_, anyhow::Error>(e.into());
        }
        apply_retention_policy(&pool, cfg.retention_period_secs).await?;
        apply_dedup_cleanup_job(&pool, cfg.retention_period_secs).await?;
        metrics.set_ready(true);
        Ok((pool, metrics))
    } => res?,
};
```

`biased` makes an already-pending cancel win deterministically over a ready I/O
branch. Registration continues below, unchanged, using `pool` / `metrics`.

## Issue 2 — detached `count(*)` leaks a connection on teardown

**Location:** `src/infra/storage/catalog_store.rs:109`

**Problem:** `spawn_catalog_size_refresh` fires a detached `tokio::spawn` running a
`count(*)` with no token and no `JoinHandle`. At shutdown the task can still hold a
pooled connection, blocking pool close. The gauge is best-effort observability,
not a contract, so it must not impede teardown.

**Fix:** Thread the cancellation token into the store and have the spawned refresh
cooperatively cancel.

- Add a `cancel: CancellationToken` field to `PgCatalogStore`; signature becomes
  `PgCatalogStore::new(pool, metrics, cancel)`.
- In the spawned refresh, `select!` the `count(*)` future against
  `cancel.cancelled()`. On cancel the query future is dropped and its connection
  returns to the pool immediately. Best-effort semantics are unchanged: a cancelled
  refresh simply leaves the previous gauge value in place (same as a failed count).
- Call sites: `src/gear.rs` passes `ctx.cancellation_token().clone()`;
  `tests/common/mod.rs` passes a fresh never-cancelled `CancellationToken`.
- `PgRecordStore` is unaffected — it does not spawn.

## Testing

- **Issue 2 (unit, no DB):** construct `PgCatalogStore` over a lazy pool with an
  already-cancelled token, invoke the refresh, and assert it returns promptly
  without querying (the catalog-size gauge keeps its prior value). Mirrors the
  lazy-store unit-test pattern referenced by REVIEW item 21.
- **Issue 1:** with `biased` + a pre-cancelled token the cancel arm wins before the
  async block is polled, so `init` returns the cancellation error with no DB I/O —
  testable if a `GearCtx` test seam is reachable. Confirm seam feasibility during
  planning; otherwise the init path is covered by inspection/review (the change is a
  mechanical `select!` wrap).

## Out of scope

- The TOOLKIT-DB ownership findings (excluded in REVIEW.md), and all other REVIEW
  findings (#3–#21).
- Adding a `RunnableCapability`/`stop` hook to the plugin — not warranted for a
  best-effort gauge; cooperative token cancellation is sufficient.

## Follow-up

On completion, mark REVIEW.md findings #1 and #2 as `✅ Resolved` with a one-line
note, matching the existing convention used for #3, #5–#8, #10–#12, #14–#16, #18.
