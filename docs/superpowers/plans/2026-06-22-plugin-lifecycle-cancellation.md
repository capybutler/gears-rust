# TimescaleDB Plugin Lifecycle Cancellation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the TimescaleDB usage-collector plugin honor the toolkit `CancellationToken` lifecycle — interrupt `init`'s startup I/O at shutdown (REVIEW #1) and stop the detached catalog-size gauge refresh from leaking a pooled connection on teardown (REVIEW #2).

**Architecture:** Issue 1 — wrap `init`'s connect/migrate/retention sequence in a `toolkit::tokio::select!` raced against `ctx.cancellation_token().cancelled()`, yielding `(pool, metrics)` out of the raced block and returning an error on cancel. Issue 2 — give `PgCatalogStore` a `CancellationToken` field and have the spawned refresh `select!` its `count(*)` against it, so cancellation drops the in-flight query and returns its connection. Both are cooperative cancellation; the plugin has no `stop` hook (only `Gear::init`), so token-driven drop is the mechanism.

**Tech Stack:** Rust, `sqlx` (Postgres), `tokio` / `tokio-util` (`CancellationToken`), `toolkit` (`Gear`, `GearCtx`), `#[tokio::test]` unit tests over a lazy `PgPool` (no Docker).

**Design spec:** `docs/superpowers/specs/2026-06-22-timescaledb-plugin-lifecycle-cancellation-design.md`

---

## File Structure

- `Cargo.toml` — **modify**. Add `tokio-util = { workspace = true }` to `[dependencies]` (the production `PgCatalogStore` struct names `CancellationToken`; `tokio` itself is dev-only and reached via `toolkit::tokio`).

> **Note on cargo commands:** the Cargo *package* name is `cf-gears-timescaledb-usage-collector-plugin` (the lib name is `timescaledb_usage_collector_plugin`). All `cargo … -p` commands below use the package name.
- `src/infra/storage/catalog_store.rs` — **modify**. Add `cancel: CancellationToken` field + `RefreshOutcome` enum + `refresh_catalog_size_cancellable`; rewire `spawn_catalog_size_refresh`; widen `PgCatalogStore::new`; add the test-module declaration.
- `src/infra/storage/catalog_store_tests.rs` — **create**. Lazy-pool unit tests for the cancellation short-circuit (no DB).
- `src/gear.rs` — **modify**. Issue 2: pass the token to `PgCatalogStore::new`. Issue 1: wrap the connect/migrate/retention sequence in the cancellation `select!`. Add the test-module declaration.
- `src/gear_tests.rs` — **create**. Unit test that a pre-cancelled token aborts `init` before startup I/O.
- `tests/common/mod.rs` — **modify**. Pass `CancellationToken::new()` to the `catalog_store` fixture builder.
- `REVIEW.md` — **modify**. Mark findings #1 and #2 `✅ Resolved`.

All units are small and self-contained: the catalog refresh's cancellation lives in one new `async` method (directly awaitable, so unit-testable); the init change is one `select!` block; nothing crosses a new module boundary.

---

## Task 1: Add the `tokio-util` dependency

**Files:**
- Modify: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/Cargo.toml`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, under `[dependencies]`, add `tokio-util` next to the other async deps (after the `toolkit-odata` line):

```toml
toolkit = { workspace = true }
toolkit-macros = { workspace = true }
toolkit-odata = { workspace = true }

tokio-util = { workspace = true }
```

(The workspace pins `tokio-util = { version = "0.7", features = ["rt"] }`.)

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p cf-gears-timescaledb-usage-collector-plugin`
Expected: builds cleanly (no usage yet; this just makes the crate name resolvable).

- [ ] **Step 3: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/Cargo.toml
git commit -m "build(usage-collector): add tokio-util dep for plugin cancellation"
```

---

## Task 2: Issue 2 — thread `CancellationToken` into the catalog gauge refresh

**Files:**
- Modify: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/catalog_store.rs`
- Create: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/catalog_store_tests.rs`
- Modify: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/gear.rs` (call site only)
- Modify: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests/common/mod.rs` (fixture only)
- Modify: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/REVIEW.md`

- [ ] **Step 1: Write the failing unit tests**

Create `src/infra/storage/catalog_store_tests.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;

use super::{PgCatalogStore, RefreshOutcome};
use crate::infra::metrics::Metrics;

/// A catalog store over a lazy pool (no connection opened) with a caller-chosen
/// cancellation token. The tiny acquire timeout keeps an accidental DB touch
/// from hanging the test.
fn lazy_store(cancel: CancellationToken) -> PgCatalogStore {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://user:pass@localhost/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    PgCatalogStore::new(pool.clone(), Arc::new(Metrics::new(pool)), cancel)
}

#[tokio::test]
async fn refresh_short_circuits_when_token_already_cancelled() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let store = lazy_store(cancel);

    // With a biased select and an already-cancelled token, the cancel arm wins
    // before the count future is polled: no query is issued, no connection is
    // checked out.
    let outcome = store.refresh_catalog_size_cancellable().await;
    assert_eq!(outcome, RefreshOutcome::Cancelled);
}

#[tokio::test]
async fn refresh_runs_the_query_when_not_cancelled() {
    // A live (never-cancelled) token: the refresh actually attempts the count.
    // Over the lazy pool the connect fails after the 50ms acquire timeout and is
    // logged at warn, but the `Ran` arm proves the query branch was taken rather
    // than short-circuited.
    let store = lazy_store(CancellationToken::new());

    let outcome = store.refresh_catalog_size_cancellable().await;
    assert_eq!(outcome, RefreshOutcome::Ran);
}
```

Add the test-module declaration at the **bottom** of `src/infra/storage/catalog_store.rs` (mirrors `record_store.rs:1182-1185`):

```rust
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "catalog_store_tests.rs"]
mod catalog_store_tests;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib catalog_store_tests`
Expected: FAIL — compile error, `RefreshOutcome` and `refresh_catalog_size_cancellable` do not exist and `PgCatalogStore::new` takes 2 args, not 3.

- [ ] **Step 3: Implement the cancellation plumbing in `catalog_store.rs`**

Add the import near the top (after the `use sqlx::{PgPool, Postgres};` line):

```rust
use tokio_util::sync::CancellationToken;
```

Add the `cancel` field to the struct (replace the existing struct definition):

```rust
#[derive(Debug, Clone)]
pub struct PgCatalogStore {
    pool: PgPool,
    metrics: Arc<Metrics>,
    /// Gear cancellation token, threaded in so the detached gauge refresh aborts
    /// its `count(*)` at shutdown instead of leaking a pooled connection.
    cancel: CancellationToken,
}
```

Widen the constructor (replace the existing `new`):

```rust
    /// Build a store over an existing connection pool.
    ///
    /// `cancel` is the gear's cancellation token
    /// ([`GearCtx::cancellation_token`](toolkit::context::GearCtx::cancellation_token));
    /// the detached catalog-size refresh races its query against it so a shutdown
    /// drops the in-flight `count(*)` and returns its connection promptly.
    #[must_use]
    pub fn new(pool: PgPool, metrics: Arc<Metrics>, cancel: CancellationToken) -> Self {
        Self {
            pool,
            metrics,
            cancel,
        }
    }
```

Replace `spawn_catalog_size_refresh` (and add the `RefreshOutcome` enum + the cancellable wrapper) so the section reads:

```rust
    /// Spawn a best-effort catalog-size gauge refresh **off** the request path.
    ///
    /// The gauge is observability, not a contract, so `create` / `delete` return
    /// without awaiting the `count(*)`; the refresh runs detached on its own
    /// cloned store handle (all fields are cheap `Arc`-backed clones). The
    /// detached task races the query against the store's cancellation token so a
    /// shutdown drops the in-flight `count(*)` and returns its connection rather
    /// than leaking a checkout.
    fn spawn_catalog_size_refresh(&self) {
        let store = self.clone();
        toolkit::tokio::spawn(async move { store.refresh_catalog_size_cancellable().await });
    }

    /// Race [`Self::refresh_catalog_size`] against the cancellation token.
    ///
    /// On cancel the `count(*)` future is dropped — returning its connection to
    /// the pool — and the gauge keeps its previous value. Returns which arm won
    /// so the unit tests can assert the short-circuit; the spawned caller ignores
    /// the result.
    async fn refresh_catalog_size_cancellable(&self) -> RefreshOutcome {
        toolkit::tokio::select! {
            biased;
            () = self.cancel.cancelled() => RefreshOutcome::Cancelled,
            () = self.refresh_catalog_size() => RefreshOutcome::Ran,
        }
    }
```

Add the `RefreshOutcome` enum just above the `impl PgCatalogStore` block (after the `gts_id_asc_order` free fn):

```rust
/// Outcome of one background catalog-size refresh. Surfaced only so the unit
/// tests can assert the cancellation short-circuit; the spawned caller discards
/// it.
#[derive(Debug, PartialEq, Eq)]
enum RefreshOutcome {
    /// The cancellation token fired before the count completed; the gauge keeps
    /// its previous value and no connection is held past cancellation.
    Cancelled,
    /// The `count(*)` ran to completion (success or a logged failure).
    Ran,
}
```

Leave `refresh_catalog_size` itself unchanged.

- [ ] **Step 4: Update the two `PgCatalogStore::new` call sites**

In `src/gear.rs`, replace the catalog-store construction (currently
`let catalog: Arc<dyn CatalogStore> = Arc::new(PgCatalogStore::new(pool.clone(), metrics));`):

```rust
        let catalog: Arc<dyn CatalogStore> = Arc::new(PgCatalogStore::new(
            pool.clone(),
            metrics,
            ctx.cancellation_token().clone(),
        ));
```

In `tests/common/mod.rs`, add the import near the other `use` lines:

```rust
use tokio_util::sync::CancellationToken;
```

and update the fixture builder (currently
`PgCatalogStore::new(pool.clone(), metrics(pool))`):

```rust
/// Convenience builder for a [`PgCatalogStore`] with its own metric handle.
#[must_use]
pub fn catalog_store(pool: &PgPool) -> PgCatalogStore {
    PgCatalogStore::new(pool.clone(), metrics(pool), CancellationToken::new())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib catalog_store_tests`
Expected: PASS (2 tests). `refresh_runs_the_query_when_not_cancelled` takes ~50ms (the lazy-pool acquire timeout).

Run: `cargo build -p cf-gears-timescaledb-usage-collector-plugin --tests`
Expected: builds cleanly — confirms `gear.rs` and the integration-test `common` fixture still compile with the 3-arg constructor.

- [ ] **Step 6: Mark REVIEW.md #2 resolved**

In `REVIEW.md`, change the **Fix** cell of row 2 (the `catalog_store.rs:109` finding) to:

```
✅ Resolved — `PgCatalogStore` now holds the gear `CancellationToken`; the detached refresh `select!`s its `count(*)` against `cancelled()`, so shutdown drops the in-flight query and returns its connection (no leaked checkout). The `ObservableGauge` alternative was rejected: the crate's gauge callbacks are synchronous and do no DB I/O.
```

- [ ] **Step 7: Lint and commit**

Run: `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets`
Expected: no warnings.

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/catalog_store.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/catalog_store_tests.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/gear.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests/common/mod.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/REVIEW.md
git commit -m "fix(usage-collector): cancel detached catalog-size refresh at shutdown (REVIEW #2)"
```

---

## Task 3: Issue 1 — race `init` startup I/O against cancellation

**Files:**
- Modify: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/gear.rs`
- Create: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/gear_tests.rs`
- Modify: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/REVIEW.md`

- [ ] **Step 1: Write the failing unit test**

Create `src/gear_tests.rs`:

```rust
use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use toolkit::{ClientHub, ConfigProvider, Gear, GearCtx};

use super::TimescaleDbUsageCollectorPlugin;

/// Minimal [`ConfigProvider`] serving one fixed gear-config JSON.
///
/// `gear_config_or_default` reads the gear node's `config` sub-object, so the
/// value must be shaped `{ "config": { ... } }`.
struct StaticConfig(serde_json::Value);

impl ConfigProvider for StaticConfig {
    fn get_gear_config(&self, _gear_name: &str) -> Option<&serde_json::Value> {
        Some(&self.0)
    }
}

#[tokio::test]
async fn init_aborts_before_startup_io_when_already_cancelled() {
    // `cfg.validate()` runs before the cancel race and requires a non-empty
    // `database_url`; the bogus DSN is never dialed because the cancelled token
    // short-circuits before `build_pool`.
    let provider = Arc::new(StaticConfig(json!({
        "config": { "database_url": "postgres://127.0.0.1:1/unused?sslmode=disable" }
    })));

    let cancel = CancellationToken::new();
    cancel.cancel();

    let ctx = GearCtx::new(
        "timescaledb-usage-collector-plugin",
        Uuid::from_u128(1),
        provider,
        Arc::new(ClientHub::default()),
        cancel,
    );

    let err = TimescaleDbUsageCollectorPlugin::default()
        .init(&ctx)
        .await
        .expect_err("a cancelled token must abort init before any startup I/O");

    assert!(
        err.to_string().contains("init cancelled during shutdown"),
        "unexpected error: {err}"
    );
}
```

Add the test-module declaration at the **bottom** of `src/gear.rs`:

```rust
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib gear_tests`
Expected: FAIL — `init` has no cancellation handling yet, so with the cancelled token it falls through to `build_pool` and returns a connection/build error; the assertion on `"init cancelled during shutdown"` fails (port 1 is refused immediately, so the run is fast).

- [ ] **Step 3: Implement the cancellation race in `init`**

In `src/gear.rs`, replace the connect/migrate/retention block (currently from the
`// Connect, migrate, and install the config-driven retention policy.` comment
through `metrics.set_ready(true);`) with:

```rust
        // Connect, migrate, and install the config-driven retention policy.
        // Race the startup-I/O sequence against the gear's cancellation token so
        // a shutdown mid-startup aborts promptly instead of blocking on each
        // call's own timeout. `Metrics::new` and the migration-failure counter
        // stay inside the raced block (the metric needs the pool; the counter
        // must still fire on a migration error), so the block yields the
        // `(pool, metrics)` it built. `ready` starts unset (0) and is flipped to
        // 1 only after migration + retention succeed.
        let cancel = ctx.cancellation_token().clone();
        let (pool, metrics) = toolkit::tokio::select! {
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

Everything below (registration payload, `registry.register`, store wiring, ClientHub
register) is unchanged and continues to use `pool` / `metrics`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib gear_tests`
Expected: PASS (1 test), returns instantly — the cancel arm wins before `build_pool` is polled.

- [ ] **Step 5: Mark REVIEW.md #1 resolved**

In `REVIEW.md`, change the **Fix** cell of row 1 (the `gear.rs:42` finding) to:

```
✅ Resolved — the connect/migrate/retention sequence runs inside a `tokio::select!` raced against `cancellation_token().cancelled()`; a shutdown mid-startup returns `init cancelled during shutdown` instead of blocking on each call's timeout. Unit test `init_aborts_before_startup_io_when_already_cancelled`.
```

Also update the **Highest-priority fixes** section at the bottom: change the
`#1 / #2` bullet to note both are now resolved, e.g.:

```
2. **#1 / #2** — ✅ Resolved: startup I/O is now cancellation-raced and the detached gauge refresh is token-cancelled.
```

- [ ] **Step 6: Lint and commit**

Run: `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets`
Expected: no warnings.

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/gear.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/gear_tests.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/REVIEW.md
git commit -m "fix(usage-collector): cancel init startup I/O at shutdown (REVIEW #1)"
```

---

## Task 4: Final verification gate

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt -p cf-gears-timescaledb-usage-collector-plugin`
Expected: no diff (or only the files we touched, already formatted).

- [ ] **Step 2: Clippy across all targets**

Run: `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Run the crate's non-Docker tests**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS — includes the new `catalog_store_tests` (2) and `gear_tests` (1) plus all existing lib unit tests.

- [ ] **Step 4: Confirm integration targets still compile**

Run: `cargo test -p cf-gears-timescaledb-usage-collector-plugin --no-run`
Expected: builds all test binaries (the `*_pg` integration tests require Docker to *run*, but must still *compile* with the 3-arg `catalog_store` fixture).

- [ ] **Step 5: Commit any formatting-only changes (if Step 1 produced a diff)**

```bash
git add -A gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "style(usage-collector): fmt after lifecycle-cancellation fixes"
```

(Skip if `git status` is clean.)

---

## Self-Review

**1. Spec coverage:**
- Spec "Issue 1 — `init` cannot be interrupted" → Task 3 (select! wrap + test). ✓
- Spec "Issue 2 — detached `count(*)` leaks a connection" → Task 2 (token field + cancellable refresh + test). ✓
- Spec "Testing — Issue 2 unit, lazy pool" → Task 2 Step 1 (`refresh_short_circuits_when_token_already_cancelled`). ✓
- Spec "Testing — Issue 1, confirm GearCtx seam feasibility" → confirmed feasible; `GearCtx::new` + a one-method `ConfigProvider` double in Task 3 Step 1. ✓
- Spec "Follow-up — mark REVIEW.md #1/#2 Resolved" → Task 2 Step 6, Task 3 Step 5. ✓
- Spec "Out of scope — no `RunnableCapability`/`stop` hook" → respected; cancellation is token-driven only. ✓

**2. Placeholder scan:** No `TBD`/`TODO`; every code step shows full code; every run step states the command and expected result. ✓

**3. Type consistency:** `RefreshOutcome { Cancelled, Ran }`, `refresh_catalog_size_cancellable(&self) -> RefreshOutcome`, and `PgCatalogStore::new(pool, metrics, cancel)` are used identically across `catalog_store.rs`, `catalog_store_tests.rs`, `gear.rs`, and `tests/common/mod.rs`. `init`'s raced block yields `(pool, metrics)` and the cancel arm returns `anyhow!("init cancelled during shutdown")`, matching the string the `gear_tests.rs` assertion checks. ✓

**Risk note:** the only non-mechanical assumption is that the macro path `toolkit::tokio::select!` resolves (consistent with the existing `toolkit::tokio::spawn` call in `catalog_store.rs`). If it does not, the fallback is `use toolkit::tokio;` at the top of the file and `tokio::select! { … }` — behavior identical.
