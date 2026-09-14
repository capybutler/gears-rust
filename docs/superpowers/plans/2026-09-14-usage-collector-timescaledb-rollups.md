# TimescaleDB Plugin — SUM/COUNT Rollups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve `SUM` and `COUNT` aggregates over whole UTC hours from an hourly
TimescaleDB continuous aggregate. Results must equal today's exact scan, and the
rollup must be kept consistent with the per-type retention sweep.

**Architecture:**
- A new migration creates the real-time continuous aggregate `usage_rollup_1h`,
  keyed `(bucket, tenant_id, gts_type_id, type_key)`, over a signed sum and count.
- Startup setup (re)applies two native refresh policies.
- `PgRecordStore::aggregate` routes an eligible query to a one-statement builder:
  whole hours come from the view, and the partial edge hours come from the ledger.
  Every other query takes the existing scan.
- The retention sweep drops a ledger chunk and deletes its rollup rows in one
  transaction.
- A 60-second monitor tick publishes refresh-policy health gauges.

**Tech Stack:** Rust 2024, `sqlx` (raw SQL), TimescaleDB 2.29.2 on PostgreSQL 18,
`cargo nextest`, `testcontainers`, `clippy::pedantic` (deny), OpenTelemetry
metrics, `toolkit-odata`.

**Spec:** `docs/superpowers/specs/2026-09-14-usage-collector-timescaledb-rollups-design.md`

## Global Constraints

**Repository and scope**
- The plugin root is `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`. Paths below are relative to it unless they start with `docs/` or `DIVERGENCES.md`.
- The package is `cf-gears-timescaledb-usage-collector-plugin`.
- Do not edit these: `gears/system/usage-collector/docs/**`, the plugin's own `docs/`, `migrations/0001_init.sql`, the root memos (`TIMESCALEDB-RETENTION.md`, `AGGREGATION-UNDER-END-ASSIGNMENT.md`, `END-ASSIGNMENT-WITH-WINDOW-END-PARTITIONING.md`, `INVESTIGATION-PRE-AGGREGATION.md`). `DIVERGENCES.md` gets only the entry-21 paragraph in Task 8.
- No SPI, SDK or gateway change.

**TimescaleDB facts (verified on 2.29.2, spec §4)**
- `refresh_continuous_aggregate` cannot run inside a transaction block. Call it in autocommit.
- A refresh policy runs within seconds of being created. A second policy with the same offsets is refused, so setup must delete the old policies first.
- `timescaledb_information.jobs.hypertable_name` for a refresh policy is the view name `usage_rollup_1h`.
- The materialisation hypertable is named by `timescaledb_information.continuous_aggregates.materialization_hypertable_{schema,name}`.
- `time_bucket(INTERVAL '1 hour', …)` aligns to Unix-epoch UTC hours.

**Rollup rules**
- View name: `usage_rollup_1h`. Columns, in order: `bucket, tenant_id, gts_type_id, type_key, sum_value, count_value`.
- Signed sum: `CASE WHEN invalidates IS NULL THEN value ELSE -value END`. Signed count: `CASE WHEN invalidates IS NULL THEN 1 ELSE -1 END`.
- A query is eligible for the rollup only when all of these hold:
  - the fold is `Sum` or `Count`;
  - `metadata_filter` is empty;
  - `group_by` is `[]` or `[TenantId]`;
  - every field named by `query.filter()` is `tenant_id`;
  - `ceil_hour(from) < floor_hour(to)`.
- A grouped rollup result drops a group whose `SUM(c) = 0`. An ungrouped `SUM` is `NULL` when `COALESCE(SUM(c),0) = 0`. An ungrouped `COUNT` is `COALESCE(SUM(c),0)`.
- The sweep never drops a ledger chunk without deleting its rollup rows in the same transaction. If the materialisation table cannot be resolved, it drops nothing.

**Configuration** (whole seconds; `TimescaleDbPluginConfig` is `deny_unknown_fields`)

| Key | Default | Rule |
| --- | --- | --- |
| `rollup_materialization_lag_secs` | `7_200` | multiple of 3600, ≥ 3600, < `rollup_live_window_secs` |
| `rollup_live_window_secs` | `259_200` | multiple of 3600, ≤ `MAX_INTERVAL_SECS` |
| `rollup_refresh_interval_secs` | `120` | `(0, MAX_INTERVAL_SECS]` |
| `rollup_history_refresh_interval_secs` | `3_600` | `(0, MAX_INTERVAL_SECS]` |
| `chunk_time_interval_secs` | unchanged `604_800` | additionally a multiple of 3600 |

**Metrics** (all under `uc_timescaledb_`, bounded labels only)

| Name | Kind | Labels |
| --- | --- | --- |
| `uc_timescaledb_aggregate_path_total` | counter | `path` = `rollup` \| `scan`; `reason` = `none` \| `fold` \| `metadata_filter` \| `group_by` \| `filter_field` \| `sub_hour_range` |
| `uc_timescaledb_rollup_rows_deleted_total` | counter | — |
| `uc_timescaledb_rollup_refresh_age_seconds` | f64 gauge | `policy` = `live` \| `history` |
| `uc_timescaledb_rollup_refresh_job_failing` | u64 gauge (0/1) | `policy` = `live` \| `history` |

Every new instrument must be added to `Metrics`, to the destructure and `vec!` in `declared_instrument_names`, and driven in `metrics_tests::every_exported_instrument_obeys_the_naming_convention`. That test applies the `_seconds` rule to histograms only, so a gauge named `…_seconds` passes.

**Rust conventions**
- No `unwrap`/`expect`/`panic!` outside tests, and no `as` numeric casts: use `try_from` / `from`.
- Test modules are sibling files, wired as `#[cfg(test)] #[cfg_attr(coverage_nightly, coverage(off))] #[path = "<name>_tests.rs"] mod <name>_tests;`.
- A file issuing raw `sqlx` SQL starts with `#![allow(unknown_lints, de0706_no_direct_sqlx)]`.
- A dynamically built SQL string is executed as `sqlx::query(AssertSqlSafe(sql))`.
- `SqlBind` does not implement `PartialEq`. Assert binds with `matches!`.

**Commits:** conventional commits scoped `timescaledb-plugin`, with no attribution trailers. Commit only the files the task names.

**Commands** (run from the repository root)

| Purpose | Command |
| --- | --- |
| Unit tests | `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib` |
| One integration binary (Docker) | `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test <name>` |
| Whole crate (Docker) | `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres` |
| Lint | `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings` |
| Format | `cargo fmt -p cf-gears-timescaledb-usage-collector-plugin` |

---

## File Structure

| File | Change | Responsibility |
| --- | --- | --- |
| `src/config.rs` (+ `config_tests.rs`) | modify | Four rollup keys; hour-multiple rules |
| `migrations/0002_usage_rollup.sql` | create | The continuous aggregate |
| `src/infra/storage/rollup_maintenance.rs` (+ `_tests.rs`) | create | Policy SQL and apply; materialisation-table lookup; refresh-job status sampling; `RollupMonitor` |
| `src/infra/storage.rs` | modify | `pub mod rollup_maintenance;` |
| `src/infra/storage/pool.rs` | modify | `apply_post_migration_setup(pool, &cfg)` also applies the policies |
| `src/infra/storage/query/translate.rs` (+ `translate_tests.rs`) | modify | `filter_fields` |
| `src/infra/storage/query/rollup.rs` (+ `rollup_tests.rs`) | create | `HourSplit`, `hour_split`, `FallbackReason`, `rollup_eligible`, `build_rollup_aggregate_sql` |
| `src/infra/storage/query.rs` | modify | `pub mod rollup;` |
| `src/infra/storage/record_store.rs` (+ `_tests.rs`) | modify | Route `aggregate`; `without_rollup` test switch |
| `src/infra/storage/retention_sweep.rs` (+ `_tests.rs`) | modify | `time_start`; transactional drop + rollup delete |
| `src/infra/metrics.rs` (+ `metrics_tests.rs`) | modify | Four instruments |
| `src/gear.rs` | modify | Background loop runs sweep and monitor ticks |
| `tests/common/mod.rs` | modify | Setup call; settle-and-remove policies; `refresh_rollup` |
| `tests/cleanup_integration_pg.rs`, `tests/retention_sweep_integration_pg.rs` | modify | New setup signature; sweep rollup tests |
| `tests/schema_integration_pg.rs` | modify | View, policies, lookup, `time_start` pins |
| `tests/rollup_aggregate_integration_pg.rs` | create | Equivalence, edge semantics, routing, freshness, monitor |
| `README.md`, `DIVERGENCES.md` | modify | Aggregate path section; config rows; entry 21 |

---
### Task 1: Rollup configuration keys

**Files:**
- Modify: `src/config.rs`, `src/config_tests.rs`, `README.md` (Configuration table and TOML example only)

**Interfaces:**
- Consumes: nothing.
- Produces: `TimescaleDbPluginConfig` fields `rollup_materialization_lag_secs: u64`, `rollup_live_window_secs: u64`, `rollup_refresh_interval_secs: u64`, `rollup_history_refresh_interval_secs: u64`; `pub const HOUR_SECS: u64 = 3_600;` in `src/config.rs`.

- [ ] **Step 1: Write the failing tests** — append to `src/config_tests.rs`:

```rust
#[test]
fn rollup_defaults_are_applied() {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.rollup_materialization_lag_secs, 7_200);
    assert_eq!(cfg.rollup_live_window_secs, 259_200);
    assert_eq!(cfg.rollup_refresh_interval_secs, 120);
    assert_eq!(cfg.rollup_history_refresh_interval_secs, 3_600);
}

/// Parse `{ "database_url": "postgres://x", <extra> }` and validate it.
fn validate_with(extra: &str) -> Result<(), String> {
    let json = format!(r#"{{ "database_url": "postgres://x", {extra} }}"#);
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(&json).unwrap();
    cfg.validate()
}

#[test]
fn validate_rejects_a_chunk_interval_that_is_not_whole_hours() {
    // Every hourly rollup bucket must lie inside one ledger chunk, or the
    // retention cut cannot delete a bucket without under-counting a neighbour.
    assert!(validate_with(r#""chunk_time_interval_secs": 5400"#).is_err());
    assert!(validate_with(r#""chunk_time_interval_secs": 7200"#).is_ok());
}

#[test]
fn validate_rejects_a_materialization_lag_that_is_not_whole_hours_or_under_one_hour() {
    assert!(validate_with(r#""rollup_materialization_lag_secs": 5400"#).is_err());
    assert!(validate_with(r#""rollup_materialization_lag_secs": 0"#).is_err());
    assert!(validate_with(r#""rollup_materialization_lag_secs": 3600"#).is_ok());
}

#[test]
fn validate_rejects_a_materialization_lag_not_below_the_live_window() {
    assert!(
        validate_with(
            r#""rollup_materialization_lag_secs": 259200, "rollup_live_window_secs": 259200"#
        )
        .is_err()
    );
}

#[test]
fn validate_rejects_a_live_window_that_is_not_whole_hours_or_beyond_the_bound() {
    assert!(validate_with(r#""rollup_live_window_secs": 262800, "rollup_materialization_lag_secs": 3600"#).is_ok());
    assert!(validate_with(r#""rollup_live_window_secs": 260000"#).is_err());
    assert!(validate_with(&format!(r#""rollup_live_window_secs": {}"#, u64::MAX - (u64::MAX % 3600))).is_err());
}

#[test]
fn validate_rejects_zero_or_unbounded_rollup_refresh_intervals() {
    for key in ["rollup_refresh_interval_secs", "rollup_history_refresh_interval_secs"] {
        assert!(validate_with(&format!(r#""{key}": 0"#)).is_err(), "{key} = 0");
        assert!(validate_with(&format!(r#""{key}": {}"#, u64::MAX)).is_err(), "{key} = MAX");
    }
}
```

- [ ] **Step 2: Run them and see them fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib config_tests`
Expected: compile error, because the new fields do not exist yet.

- [ ] **Step 3: Implement** in `src/config.rs`.

(a) Add these fields after `retention_sweep_interval_secs`:

```rust
    /// Seconds before now that the rollup's newest materialised bucket ends.
    /// Newer buckets are answered from the ledger by real-time aggregation, so
    /// a refresh never recomputes the hours ingestion is still writing. A
    /// multiple of 3600, at least 3600, and below `rollup_live_window_secs`.
    pub rollup_materialization_lag_secs: u64,
    /// How far back the frequent (live) refresh policy reaches, in seconds.
    /// Writes older than this are picked up by the history policy instead. A
    /// multiple of 3600.
    pub rollup_live_window_secs: u64,
    /// Seconds between runs of the live refresh policy.
    pub rollup_refresh_interval_secs: u64,
    /// Seconds between runs of the history refresh policy.
    pub rollup_history_refresh_interval_secs: u64,
```

(b) Add these to `Default`, after `retention_sweep_interval_secs: 3_600,`:

```rust
            rollup_materialization_lag_secs: 7_200,
            rollup_live_window_secs: 259_200,
            rollup_refresh_interval_secs: 120,
            rollup_history_refresh_interval_secs: 3_600,
```

(c) Add this beside `MAX_INTERVAL_SECS`:

```rust
/// One rollup bucket, in seconds. Every interval a bucket must nest inside is a
/// multiple of it.
pub const HOUR_SECS: u64 = 3_600;
```

(d) In `validate`, replace the `chunk_time_interval_secs` check with the one below, then append the rest before `Ok(())`. Extend the `# Errors` doc list with "an hour-aligned setting that is not a multiple of 3600, a materialization lag under one hour or not below the live window, or a rollup interval outside `(0, MAX_INTERVAL_SECS]`".

```rust
        if self.chunk_time_interval_secs == 0
            || self.chunk_time_interval_secs > MAX_INTERVAL_SECS
            || self.chunk_time_interval_secs % HOUR_SECS != 0
        {
            return Err(format!(
                "chunk_time_interval_secs must be a multiple of {HOUR_SECS} in (0, {MAX_INTERVAL_SECS}] \
                 (every hourly rollup bucket must lie inside one chunk)"
            ));
        }
```

```rust
        if self.rollup_live_window_secs == 0
            || self.rollup_live_window_secs > MAX_INTERVAL_SECS
            || self.rollup_live_window_secs % HOUR_SECS != 0
        {
            return Err(format!(
                "rollup_live_window_secs must be a multiple of {HOUR_SECS} in (0, {MAX_INTERVAL_SECS}]"
            ));
        }
        if self.rollup_materialization_lag_secs < HOUR_SECS
            || self.rollup_materialization_lag_secs % HOUR_SECS != 0
            || self.rollup_materialization_lag_secs >= self.rollup_live_window_secs
        {
            return Err(format!(
                "rollup_materialization_lag_secs must be a multiple of {HOUR_SECS}, at least \
                 {HOUR_SECS}, and below rollup_live_window_secs"
            ));
        }
        for (key, value) in [
            ("rollup_refresh_interval_secs", self.rollup_refresh_interval_secs),
            ("rollup_history_refresh_interval_secs", self.rollup_history_refresh_interval_secs),
        ] {
            if value == 0 || value > MAX_INTERVAL_SECS {
                return Err(format!("{key} must be in (0, {MAX_INTERVAL_SECS}] (100 years)"));
            }
        }
```

The existing `validate_rejects_a_chunk_time_interval_beyond_the_interval_bound` still passes: `u64::MAX` fails the bound check first.

- [ ] **Step 4: Run the tests and see them pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib config_tests`
Expected: PASS.

- [ ] **Step 5: README.** In the Configuration table, after the `retention_sweep_interval_secs` row, add:

```markdown
| `rollup_materialization_lag_secs` | `7200` (2h) | Buckets newer than this are answered from the ledger rather than materialised. Multiple of 3600, at least 3600, below `rollup_live_window_secs`. See [Aggregate path](#aggregate-path). |
| `rollup_live_window_secs` | `259200` (3d) | Reach of the frequent refresh policy. Size it to the gateway's live past tolerance (48h by default). Multiple of 3600. |
| `rollup_refresh_interval_secs` | `120` | Seconds between live refresh-policy runs. |
| `rollup_history_refresh_interval_secs` | `3600` (1h) | Seconds between history refresh-policy runs (backfill, old invalidations). |
```

Change the `chunk_time_interval_secs` description to: `Time width of new ledger chunks; a multiple of 3600; applies to chunks created afterwards.` In the TOML example, add these after `retention_sweep_interval_secs = 3600`:

```toml
rollup_materialization_lag_secs = 7200
rollup_live_window_secs = 259200
rollup_refresh_interval_secs = 120
rollup_history_refresh_interval_secs = 3600
```

- [ ] **Step 6: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/config.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/config_tests.rs \
        gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/README.md
git commit -m "feat(timescaledb-plugin): configure the rollup's lag, live window and refresh intervals"
```

---

### Task 2: The continuous aggregate and its refresh policies

**Files:**
- Create: `migrations/0002_usage_rollup.sql`
- Create: `src/infra/storage/rollup_maintenance.rs`, `src/infra/storage/rollup_maintenance_tests.rs`
- Modify: `src/infra/storage.rs`, `src/infra/storage/pool.rs`, `src/gear.rs`
- Modify: `tests/common/mod.rs`, `tests/cleanup_integration_pg.rs`, `tests/retention_sweep_integration_pg.rs`, `tests/schema_integration_pg.rs`

**Interfaces:**
- Consumes: Task 1's config fields.
- Produces:
  - `pub const ROLLUP_VIEW: &str = "usage_rollup_1h";`
  - `pub const DELETE_ROLLUP_POLICIES_SQL: &str`, `pub const ADD_LIVE_POLICY_SQL: &str`, `pub const ADD_HISTORY_POLICY_SQL: &str`
  - `pub async fn apply_rollup_policies(pool: &PgPool, cfg: &TimescaleDbPluginConfig) -> Result<(), sqlx::Error>`
  - `pub async fn apply_post_migration_setup(pool: &PgPool, cfg: &TimescaleDbPluginConfig) -> Result<(), sqlx::Error>`. This replaces the `(pool, chunk_time_interval_secs, type_key_slice_width)` form.
  - In `tests/common/mod.rs`: `pub async fn refresh_rollup(pool: &PgPool)`, and `TsHarness.cfg: TimescaleDbPluginConfig`.

- [ ] **Step 1: Write the failing schema tests.** Append to `tests/schema_integration_pg.rs`, and add `use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;` to its imports:

```rust
/// The rollup is a real-time continuous aggregate over the grain the read path
/// and the retention cut both rely on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rollup_is_a_real_time_continuous_aggregate_over_the_grain() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let materialized_only: bool = sqlx::query_scalar(
        "SELECT materialized_only FROM timescaledb_information.continuous_aggregates \
         WHERE view_schema = current_schema() AND view_name = 'usage_rollup_1h'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_rollup_1h must be a continuous aggregate");
    assert!(!materialized_only, "real-time aggregation must be on");

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT attname::text FROM pg_attribute \
         WHERE attrelid = 'usage_rollup_1h'::regclass AND attnum > 0 AND NOT attisdropped \
         ORDER BY attnum",
    )
    .fetch_all(&h.pool)
    .await
    .expect("view columns");
    assert_eq!(
        columns,
        ["bucket", "tenant_id", "gts_type_id", "type_key", "sum_value", "count_value"]
    );
}

/// Setup registers exactly the two configured policies, and re-running it
/// replaces them rather than adding more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_registers_exactly_the_two_configured_refresh_policies() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let cfg = timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig {
        rollup_materialization_lag_secs: 10_800,
        rollup_live_window_secs: 172_800,
        rollup_refresh_interval_secs: 300,
        rollup_history_refresh_interval_secs: 7_200,
        ..h.cfg.clone()
    };
    apply_post_migration_setup(&h.pool, &cfg).await.expect("setup once");
    apply_post_migration_setup(&h.pool, &cfg).await.expect("setup twice");

    // (start offset secs or NULL, end offset secs, schedule secs), live first.
    let policies: Vec<(Option<f64>, f64, f64)> = sqlx::query_as(
        "SELECT EXTRACT(EPOCH FROM (config->>'start_offset')::interval)::float8, \
                EXTRACT(EPOCH FROM (config->>'end_offset')::interval)::float8, \
                EXTRACT(EPOCH FROM schedule_interval)::float8 \
         FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_refresh_continuous_aggregate' \
           AND hypertable_schema = current_schema() AND hypertable_name = 'usage_rollup_1h' \
         ORDER BY (config->>'start_offset') IS NULL, job_id",
    )
    .fetch_all(&h.pool)
    .await
    .expect("policy rows");
    assert_eq!(
        policies,
        vec![
            (Some(172_800.0), 10_800.0, 300.0),
            (None, 172_800.0, 7_200.0),
        ],
        "one live policy and one history policy, replaced on re-run"
    );
}
```

- [ ] **Step 2: Run them and see them fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test schema_integration_pg`
Expected: compile error, because `h.cfg` and the new setup signature do not exist yet.

- [ ] **Step 3: Create `migrations/0002_usage_rollup.sql`**

```sql
-- TimescaleDB Usage Collector storage backend — the SUM/COUNT rollup.
--
-- An hourly real-time continuous aggregate over the ledger. The aggregate read
-- path serves whole UTC hours of a SUM or COUNT query from it; everything else
-- reads the ledger. It is plugin-internal: no SPI shape names it.
--
-- Signed netting is exact. The scan excludes a withdrawn pair; this view adds
-- +value/+1 for a record and -value/-1 for its invalidation instead. The two
-- agree because (1) an invalidation copies its target's window_end, tenant_id
-- and type, so both land in one row here; (2) a record carries at most one
-- invalidation (usage_records_one_invalidation_uniq plus the gateway;
-- DIVERGENCES.md entry 21); (3) the gateway admits an invalidation only for an
-- existing record; (4) the pair shares a chunk, so retention drops it together.
--
-- type_key adds no rows (it is a function of gts_type_id). It is in the grain
-- so reads and the retention sweep's rollup cut can prune by type.
--
-- Refresh policies are not created here: startup setup applies them from
-- configuration (pool::apply_post_migration_setup).
CREATE MATERIALIZED VIEW IF NOT EXISTS usage_rollup_1h
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '1 hour', window_end) AS bucket,
       tenant_id,
       gts_type_id,
       type_key,
       SUM(CASE WHEN invalidates IS NULL THEN value ELSE -value END) AS sum_value,
       SUM(CASE WHEN invalidates IS NULL THEN 1 ELSE -1 END)::bigint AS count_value
FROM usage_records
GROUP BY 1, 2, 3, 4
WITH NO DATA;
```

- [ ] **Step 4: Create `src/infra/storage/rollup_maintenance.rs`**

```rust
//! Everything that maintains the `usage_rollup_1h` continuous aggregate apart
//! from reading it: its refresh policies here, and (in later tasks) the lookup
//! of its materialisation table and the sampling of its refresh jobs.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (see
// `record_store.rs`).
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use sqlx::PgPool;

use crate::config::TimescaleDbPluginConfig;

/// The rollup view `migrations/0002_usage_rollup.sql` creates.
pub const ROLLUP_VIEW: &str = "usage_rollup_1h";

/// Deletes every refresh policy of the rollup. For a refresh policy
/// `timescaledb_information.jobs.hypertable_name` is the view itself (verified
/// on 2.29.2), and a policy with the same offsets as an existing one is refused,
/// so setup deletes before it adds.
pub const DELETE_ROLLUP_POLICIES_SQL: &str = "SELECT delete_job(job_id) \
     FROM timescaledb_information.jobs \
     WHERE proc_name = 'policy_refresh_continuous_aggregate' \
     AND hypertable_schema = current_schema() AND hypertable_name = 'usage_rollup_1h'";

/// The live policy: `$1` live window, `$2` materialisation lag, `$3` schedule,
/// all in seconds.
pub const ADD_LIVE_POLICY_SQL: &str = "SELECT add_continuous_aggregate_policy('usage_rollup_1h', \
     start_offset => make_interval(secs => $1::double precision), \
     end_offset => make_interval(secs => $2::double precision), \
     schedule_interval => make_interval(secs => $3::double precision))";

/// The history policy: everything older than the live window. `$1` live
/// window, `$2` schedule, in seconds. A refresh over a range with nothing
/// invalidated is a no-op, so an unbounded start costs nothing steady-state.
pub const ADD_HISTORY_POLICY_SQL: &str = "SELECT add_continuous_aggregate_policy('usage_rollup_1h', \
     start_offset => NULL, \
     end_offset => make_interval(secs => $1::double precision), \
     schedule_interval => make_interval(secs => $2::double precision))";

/// A config value in seconds as the `double precision` `make_interval` takes.
/// Every value is validated `<= MAX_INTERVAL_SECS` (100 years), well inside
/// `i64`, so the saturation is unreachable in practice.
fn secs(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Replace the rollup's refresh policies with the two `cfg` describes.
/// Idempotent: a restart with changed values applies them.
///
/// # Errors
/// Returns `sqlx::Error` if any statement fails.
pub async fn apply_rollup_policies(
    pool: &PgPool,
    cfg: &TimescaleDbPluginConfig,
) -> Result<(), sqlx::Error> {
    sqlx::query(DELETE_ROLLUP_POLICIES_SQL).execute(pool).await?;
    sqlx::query(ADD_LIVE_POLICY_SQL)
        .bind(secs(cfg.rollup_live_window_secs))
        .bind(secs(cfg.rollup_materialization_lag_secs))
        .bind(secs(cfg.rollup_refresh_interval_secs))
        .execute(pool)
        .await?;
    sqlx::query(ADD_HISTORY_POLICY_SQL)
        .bind(secs(cfg.rollup_live_window_secs))
        .bind(secs(cfg.rollup_history_refresh_interval_secs))
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "rollup_maintenance_tests.rs"]
mod rollup_maintenance_tests;
```

`$1::double precision` receiving an `i64` bind matches what `pool::apply_partitioning` already does.

Create `src/infra/storage/rollup_maintenance_tests.rs`:

```rust
use super::{ADD_HISTORY_POLICY_SQL, ADD_LIVE_POLICY_SQL, DELETE_ROLLUP_POLICIES_SQL, ROLLUP_VIEW};

#[test]
fn every_policy_statement_names_the_rollup_view() {
    for sql in [DELETE_ROLLUP_POLICIES_SQL, ADD_LIVE_POLICY_SQL, ADD_HISTORY_POLICY_SQL] {
        assert!(sql.contains(ROLLUP_VIEW), "{sql}");
    }
}

#[test]
fn the_policy_delete_is_scoped_to_the_rollups_refresh_jobs() {
    assert!(DELETE_ROLLUP_POLICIES_SQL.contains("proc_name = 'policy_refresh_continuous_aggregate'"));
    assert!(DELETE_ROLLUP_POLICIES_SQL.contains("hypertable_schema = current_schema()"));
    assert!(DELETE_ROLLUP_POLICIES_SQL.contains("hypertable_name = 'usage_rollup_1h'"));
}

#[test]
fn the_history_policy_has_no_start_and_the_live_policy_does() {
    assert!(ADD_HISTORY_POLICY_SQL.contains("start_offset => NULL"));
    assert!(ADD_LIVE_POLICY_SQL.contains("start_offset => make_interval(secs => $1"));
}
```

In `src/infra/storage.rs`, add `pub mod rollup_maintenance;` after `pub mod retention_sweep;`.

- [ ] **Step 5: Change the setup signature** in `src/infra/storage/pool.rs`.

(a) Add `use crate::infra::storage::rollup_maintenance::apply_rollup_policies;`.

(b) Replace the `apply_post_migration_setup` signature and its call to `apply_partitioning`:

```rust
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
```

Keep the unlock block and `result` return unchanged. Change its doc's first line to "Run the post-migration partitioning and rollup-policy setup under a database advisory lock …".

(c) In `src/gear.rs`, replace `apply_post_migration_setup(&pool, cfg.chunk_time_interval_secs, cfg.type_key_slice_width)` with `apply_post_migration_setup(&pool, &cfg)`.

- [ ] **Step 6: Update the test harness** in `tests/common/mod.rs`.

(a) Add `pub cfg: TimescaleDbPluginConfig,` to `TsHarness`.

(b) Replace the `MIGRATOR.run` / `apply_post_migration_setup` / `Ok(TsHarness{..})` tail of `bring_up_with` with:

```rust
    MIGRATOR.run(&pool).await?;
    apply_post_migration_setup(&pool, &cfg).await?;
    settle_and_remove_rollup_policies(&pool).await?;
    Ok(TsHarness {
        pool,
        cfg,
        _container: container,
    })
}

/// Wait for the two refresh policies' first run, which `TimescaleDB` starts
/// within seconds of creating them, then delete them. A background refresh
/// racing a test would make "stale until refreshed" assertions flaky; tests
/// refresh explicitly through [`refresh_rollup`] instead. A test about the
/// policies re-applies setup.
async fn settle_and_remove_rollup_policies(pool: &PgPool) -> anyhow::Result<()> {
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
```

(c) In `tests/cleanup_integration_pg.rs`, clone the harness config into each task:

```rust
    for _ in 0..8u32 {
        let pool = h.pool.clone();
        let cfg = h.cfg.clone();
        tasks.push(tokio::spawn(async move {
            apply_post_migration_setup(&pool, &cfg).await
        }));
    }
```

(d) In `tests/retention_sweep_integration_pg.rs::a_shared_slice_is_held_to_its_longest_retention`, replace the setup call with:

```rust
    let widened = timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig {
        type_key_slice_width: 4,
        ..h.cfg.clone()
    };
    apply_post_migration_setup(&h.pool, &widened)
        .await
        .expect("widen the type-key slice");
```

That re-applies the policies. They can run while the test writes, but this test reads no rollup, so it is unaffected.

- [ ] **Step 7: Run the schema, cleanup and retention suites**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test schema_integration_pg --test cleanup_integration_pg --test retention_sweep_integration_pg`
Expected: PASS, including the two new schema tests.

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS.

- [ ] **Step 8: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
P=gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git add $P/migrations/0002_usage_rollup.sql $P/src/infra/storage.rs \
        $P/src/infra/storage/rollup_maintenance.rs $P/src/infra/storage/rollup_maintenance_tests.rs \
        $P/src/infra/storage/pool.rs $P/src/gear.rs $P/tests/common/mod.rs \
        $P/tests/cleanup_integration_pg.rs $P/tests/retention_sweep_integration_pg.rs \
        $P/tests/schema_integration_pg.rs
git commit -m "feat(timescaledb-plugin): materialise an hourly SUM/COUNT rollup with live and history refresh policies"
```

---

### Task 3: Decide whether a query can be served from the rollup

**Files:**
- Modify: `src/infra/storage/query/translate.rs`, `src/infra/storage/query/translate_tests.rs`
- Create: `src/infra/storage/query/rollup.rs`, `src/infra/storage/query/rollup_tests.rs`
- Modify: `src/infra/storage/query.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub fn filter_fields(expr: &ast::Expr) -> Result<BTreeSet<&'static str>, String>` in `query/translate.rs`.
  - In `query/rollup.rs`:
    - `pub const BUCKET_NANOS: i128 = 3_600_000_000_000;`
    - `pub struct HourSplit { pub from: OffsetDateTime, pub whole_from: OffsetDateTime, pub whole_to: OffsetDateTime, pub to: OffsetDateTime }` (`Debug, Clone, Copy, PartialEq, Eq`), with `pub fn has_lower_edge(self) -> bool` and `pub fn has_upper_edge(self) -> bool`.
    - `pub fn hour_split(range: TimeRange) -> Option<HourSplit>`
    - `pub enum FallbackReason { Fold, MetadataFilter, GroupBy, FilterField, SubHourRange }` (`Debug, Clone, Copy, PartialEq, Eq`), with `pub const fn as_label(self) -> &'static str`.
    - `pub fn rollup_eligible(fold: AggregationFold, filter: Option<&ast::Expr>, metadata_filter: &[MetadataFilter], group_by: &[AggregationDimension], range: TimeRange) -> Result<HourSplit, FallbackReason>`

- [ ] **Step 1: Write the failing `filter_fields` tests.** Append to `src/infra/storage/query/translate_tests.rs`, and add `filter_fields` to the `use super::{…}` list:

```rust
// ── filter_fields ──────────────────────────────────────────────────────────

fn parsed(raw: &str) -> toolkit_odata::ast::Expr {
    toolkit_odata::parse_filter_string(raw)
        .unwrap_or_else(|e| panic!("the test's own filter must parse: {e}"))
        .into_expr()
}

#[test]
fn filter_fields_names_every_field_a_nested_filter_touches() {
    let expr = parsed(
        "tenant_id eq 11111111-1111-1111-1111-111111111111 \
         or (origin eq 'live' and not (resource_id eq 'r1'))",
    );
    let fields = filter_fields(&expr).expect("a schema filter resolves");
    assert_eq!(
        fields.into_iter().collect::<Vec<_>>(),
        ["origin", "resource_id", "tenant_id"]
    );
}

#[test]
fn filter_fields_reads_the_field_of_an_in_list() {
    let expr = parsed(
        "tenant_id in (11111111-1111-1111-1111-111111111111, 22222222-2222-2222-2222-222222222222)",
    );
    assert_eq!(
        filter_fields(&expr).expect("resolves").into_iter().collect::<Vec<_>>(),
        ["tenant_id"]
    );
}

#[test]
fn filter_fields_refuses_a_field_off_the_schema() {
    assert!(filter_fields(&parsed("gts_type_id eq 'x'")).is_err());
}
```

- [ ] **Step 2: Run them and see them fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib translate_tests::filter_fields`
Expected: compile error, because `filter_fields` does not exist yet.

- [ ] **Step 3: Implement `filter_fields`** in `translate.rs`. Add `use std::collections::BTreeSet;`, then add after `translate_scope`:

```rust
/// Every filter-field name `expr` references, resolved through the same
/// schema gate [`translate_scope`] applies first.
///
/// The aggregate path uses it to decide whether a query's predicate can be
/// applied to rollup rows, which it can only when every name is a rollup grain
/// column. It is the whole of that decision's view of the filter, so it walks
/// every node kind, including under `not`.
///
/// # Errors
///
/// Returns `invalid read predicate: …` when an identifier is off the schema,
/// the same refusal [`translate_scope`] would give.
pub fn filter_fields(expr: &ast::Expr) -> Result<BTreeSet<&'static str>, String> {
    let node = convert_expr_to_filter_node::<UsageRecordFilterField>(expr)
        .map_err(|e| format!("invalid read predicate: {e}"))?;
    let mut names = BTreeSet::new();
    collect_filter_fields(&node, &mut names);
    Ok(names)
}

fn collect_filter_fields<F: FilterField>(node: &FilterNode<F>, out: &mut BTreeSet<&'static str>) {
    match node {
        FilterNode::Binary { field, .. } | FilterNode::InList { field, .. } => {
            out.insert(field.name());
        }
        FilterNode::Composite { children, .. } => {
            for child in children {
                collect_filter_fields(child, out);
            }
        }
        FilterNode::Not(inner) => collect_filter_fields(inner, out),
    }
}
```

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib translate_tests::filter_fields`
Expected: PASS.

- [ ] **Step 4: Write the failing eligibility and split tests.** Create `src/infra/storage/query/rollup_tests.rs`:

```rust
#![allow(clippy::panic)]

use time::OffsetDateTime;
use toolkit_odata::ast;
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, MetadataFilter, MetadataKey, TimeRange,
};

use super::{FallbackReason, HourSplit, hour_split, rollup_eligible};

/// `2023-11-14T22:00:00Z`, an exact hour.
const H0: i64 = 1_699_999_200;
const HOUR: i64 = 3_600;

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("valid instant")
}

fn range(from: i64, to: i64) -> TimeRange {
    TimeRange::new(at(from), at(to)).expect("ordered range")
}

fn parsed(raw: &str) -> ast::Expr {
    toolkit_odata::parse_filter_string(raw)
        .unwrap_or_else(|e| panic!("the test's own filter must parse: {e}"))
        .into_expr()
}

// ── hour_split ─────────────────────────────────────────────────────────────

#[test]
fn an_aligned_range_has_no_edges() {
    let split = hour_split(range(H0, H0 + 24 * HOUR)).expect("whole hours");
    assert_eq!(split.whole_from, at(H0));
    assert_eq!(split.whole_to, at(H0 + 24 * HOUR));
    assert!(!split.has_lower_edge() && !split.has_upper_edge());
}

#[test]
fn an_unaligned_range_rounds_inward_on_both_sides() {
    let split = hour_split(range(H0 + 800, H0 + 5 * HOUR + 420)).expect("whole hours");
    assert_eq!(
        split,
        HourSplit {
            from: at(H0 + 800),
            whole_from: at(H0 + HOUR),
            whole_to: at(H0 + 5 * HOUR),
            to: at(H0 + 5 * HOUR + 420),
        }
    );
    assert!(split.has_lower_edge() && split.has_upper_edge());
}

#[test]
fn one_aligned_bound_leaves_one_edge() {
    let split = hour_split(range(H0, H0 + 2 * HOUR + 1)).expect("whole hours");
    assert!(!split.has_lower_edge() && split.has_upper_edge());
}

#[test]
fn exactly_one_hour_is_one_whole_bucket() {
    let split = hour_split(range(H0 + HOUR, H0 + 2 * HOUR)).expect("one bucket");
    assert_eq!((split.whole_from, split.whole_to), (at(H0 + HOUR), at(H0 + 2 * HOUR)));
}

#[test]
fn no_whole_hour_means_no_split() {
    assert_eq!(hour_split(range(H0 + 600, H0 + 3_000)), None, "inside one hour");
    assert_eq!(hour_split(range(H0 + 3_000, H0 + HOUR + 600)), None, "crosses a boundary");
}

#[test]
fn a_sub_second_lower_bound_rounds_up_to_the_next_hour() {
    let from = at(H0) + time::Duration::nanoseconds(1);
    let split = hour_split(TimeRange::new(from, at(H0 + 3 * HOUR)).expect("ordered"))
        .expect("whole hours");
    assert_eq!(split.whole_from, at(H0 + HOUR));
    assert_eq!(split.from, from, "the edge keeps the exact bound");
}

#[test]
fn a_bound_before_the_epoch_floors_toward_minus_infinity() {
    let split = hour_split(range(-5_400, 5_400)).expect("whole hours");
    assert_eq!((split.whole_from, split.whole_to), (at(-3_600), at(3_600)));
}

// ── rollup_eligible ────────────────────────────────────────────────────────

fn aligned() -> TimeRange {
    range(H0, H0 + 24 * HOUR)
}

#[test]
fn sum_and_count_over_whole_hours_with_a_tenant_filter_are_eligible() {
    let tenant_filters = [
        parsed("tenant_id eq 11111111-1111-1111-1111-111111111111"),
        parsed(
            "tenant_id eq 11111111-1111-1111-1111-111111111111 \
             or tenant_id eq 22222222-2222-2222-2222-222222222222",
        ),
        parsed("tenant_id in (11111111-1111-1111-1111-111111111111)"),
    ];
    for fold in [AggregationFold::Sum, AggregationFold::Count] {
        for group_by in [vec![], vec![AggregationDimension::TenantId]] {
            assert!(rollup_eligible(fold, None, &[], &group_by, aligned()).is_ok());
            for f in &tenant_filters {
                assert!(
                    rollup_eligible(fold, Some(f), &[], &group_by, aligned()).is_ok(),
                    "{fold:?} {group_by:?} {f:?}"
                );
            }
        }
    }
}

#[test]
fn the_other_folds_fall_back() {
    for fold in [AggregationFold::Min, AggregationFold::Max, AggregationFold::Latest] {
        assert_eq!(
            rollup_eligible(fold, None, &[], &[], aligned()),
            Err(FallbackReason::Fold)
        );
    }
}

#[test]
fn a_metadata_filter_falls_back() {
    let mf = [MetadataFilter::new("region", ["eu"]).expect("valid filter")];
    assert_eq!(
        rollup_eligible(AggregationFold::Sum, None, &mf, &[], aligned()),
        Err(FallbackReason::MetadataFilter)
    );
}

#[test]
fn grouping_by_anything_but_tenant_alone_falls_back() {
    let metadata = AggregationDimension::Metadata(MetadataKey::new("tier").expect("valid key"));
    for group_by in [
        vec![AggregationDimension::ResourceId],
        vec![AggregationDimension::ResourceType],
        vec![AggregationDimension::SubjectId],
        vec![AggregationDimension::SubjectType],
        vec![metadata],
        vec![AggregationDimension::TenantId, AggregationDimension::ResourceType],
    ] {
        assert_eq!(
            rollup_eligible(AggregationFold::Sum, None, &[], &group_by, aligned()),
            Err(FallbackReason::GroupBy),
            "{group_by:?}"
        );
    }
}

#[test]
fn a_filter_naming_a_field_outside_the_grain_falls_back() {
    for raw in [
        "origin eq 'live'",
        "entry_type eq 'record'",
        "invalidates eq 11111111-1111-1111-1111-111111111111",
        "resource_id eq 'r1'",
        "subject_id eq 's1'",
        "tenant_id eq 11111111-1111-1111-1111-111111111111 or origin eq 'backfill'",
    ] {
        assert_eq!(
            rollup_eligible(AggregationFold::Count, Some(&parsed(raw)), &[], &[], aligned()),
            Err(FallbackReason::FilterField),
            "{raw}"
        );
    }
}

#[test]
fn a_range_without_a_whole_hour_falls_back() {
    assert_eq!(
        rollup_eligible(AggregationFold::Sum, None, &[], &[], range(H0 + 60, H0 + 3_000)),
        Err(FallbackReason::SubHourRange)
    );
}

#[test]
fn every_fallback_reason_has_its_own_label() {
    let labels = [
        FallbackReason::Fold,
        FallbackReason::MetadataFilter,
        FallbackReason::GroupBy,
        FallbackReason::FilterField,
        FallbackReason::SubHourRange,
    ]
    .map(FallbackReason::as_label);
    assert_eq!(labels, ["fold", "metadata_filter", "group_by", "filter_field", "sub_hour_range"]);
}
```

- [ ] **Step 5: Implement `src/infra/storage/query/rollup.rs`.** Add `pub mod rollup;` to `src/infra/storage/query.rs` after `pub mod keyset;`.

```rust
//! The rollup read path: which aggregate queries `usage_rollup_1h` can answer
//! exactly, and how their range splits into whole hours and partial edges.
//!
//! A query is eligible only when every predicate and grouping it carries is a
//! rollup grain column, so filtering rollup rows gives what filtering ledger
//! rows would. `origin`, `entry_type` and `invalidates` are never in the grain,
//! because each can tell a record from its invalidation, and signed netting is
//! exact only when a filter selects both or neither.

use time::OffsetDateTime;
use toolkit_odata::ast;
use usage_collector_sdk::{AggregationDimension, AggregationFold, MetadataFilter, TimeRange};

use super::translate::filter_fields;

/// One rollup bucket, in nanoseconds. `time_bucket(INTERVAL '1 hour', …)`
/// aligns buckets to Unix-epoch UTC hours, and so does this.
pub const BUCKET_NANOS: i128 = 3_600_000_000_000;

/// The filter fields a rollup row carries. `gts_type_id` is also in the grain,
/// but it is a typed parameter and never a filter field.
const GRAIN_FILTER_FIELDS: [&str; 1] = ["tenant_id"];

/// A read range split at whole hours: `[from, whole_from)` and
/// `[whole_to, to)` are the ledger-read edges, `[whole_from, whole_to)` the
/// rollup-read middle. `whole_from < whole_to` always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HourSplit {
    pub from: OffsetDateTime,
    pub whole_from: OffsetDateTime,
    pub whole_to: OffsetDateTime,
    pub to: OffsetDateTime,
}

impl HourSplit {
    /// Whether `[from, whole_from)` holds any instant.
    #[must_use]
    pub fn has_lower_edge(self) -> bool {
        self.from < self.whole_from
    }

    /// Whether `[whole_to, to)` holds any instant.
    #[must_use]
    pub fn has_upper_edge(self) -> bool {
        self.whole_to < self.to
    }
}

/// Why a query is served by the ledger scan rather than the rollup. The label
/// is the bounded `reason` of `uc_timescaledb_aggregate_path_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    Fold,
    MetadataFilter,
    GroupBy,
    FilterField,
    SubHourRange,
}

impl FallbackReason {
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Fold => "fold",
            Self::MetadataFilter => "metadata_filter",
            Self::GroupBy => "group_by",
            Self::FilterField => "filter_field",
            Self::SubHourRange => "sub_hour_range",
        }
    }
}

fn floor_hour(t: OffsetDateTime) -> Option<OffsetDateTime> {
    let n = t.unix_timestamp_nanos();
    OffsetDateTime::from_unix_timestamp_nanos(n.div_euclid(BUCKET_NANOS) * BUCKET_NANOS).ok()
}

fn ceil_hour(t: OffsetDateTime) -> Option<OffsetDateTime> {
    if t.unix_timestamp_nanos().rem_euclid(BUCKET_NANOS) == 0 {
        Some(t)
    } else {
        floor_hour(t)?.checked_add(time::Duration::hours(1))
    }
}

/// Split `range` at whole UTC hours, or `None` when it covers no whole hour.
///
/// The edges keep the exact bounds, so the edge scan binds the same instants the
/// full scan would, and the two paths round a sub-microsecond bound the same way.
#[must_use]
pub fn hour_split(range: TimeRange) -> Option<HourSplit> {
    let from = range.lower_inclusive();
    let to = range.upper_exclusive();
    let whole_from = ceil_hour(from)?;
    let whole_to = floor_hour(to)?;
    (whole_from < whole_to).then_some(HourSplit {
        from,
        whole_from,
        whole_to,
        to,
    })
}

/// Whether the rollup can answer this aggregate exactly, and its hour split if
/// so. The checks run in the order of [`FallbackReason`]'s variants, so a
/// query with several disqualifications reports the first.
///
/// # Errors
///
/// Returns the first [`FallbackReason`] that applies.
pub fn rollup_eligible(
    fold: AggregationFold,
    filter: Option<&ast::Expr>,
    metadata_filter: &[MetadataFilter],
    group_by: &[AggregationDimension],
    range: TimeRange,
) -> Result<HourSplit, FallbackReason> {
    if !matches!(fold, AggregationFold::Sum | AggregationFold::Count) {
        return Err(FallbackReason::Fold);
    }
    if !metadata_filter.is_empty() {
        return Err(FallbackReason::MetadataFilter);
    }
    if !matches!(group_by, [] | [AggregationDimension::TenantId]) {
        return Err(FallbackReason::GroupBy);
    }
    if let Some(expr) = filter {
        // A filter off the schema falls back too, so the scan reports the same
        // translation error it always has.
        let fields = filter_fields(expr).map_err(|_| FallbackReason::FilterField)?;
        if !fields.iter().all(|f| GRAIN_FILTER_FIELDS.contains(f)) {
            return Err(FallbackReason::FilterField);
        }
    }
    hour_split(range).ok_or(FallbackReason::SubHourRange)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "rollup_tests.rs"]
mod rollup_tests;
```

- [ ] **Step 6: Run the tests and see them pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib rollup_tests translate_tests`
Expected: PASS.

- [ ] **Step 7: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
P=gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage
git add $P/query.rs $P/query/translate.rs $P/query/translate_tests.rs $P/query/rollup.rs $P/query/rollup_tests.rs
git commit -m "feat(timescaledb-plugin): decide which aggregates the rollup can answer exactly"
```

---

### Task 4: Build the rollup aggregate statement

**Files:**
- Modify: `src/infra/storage/query/rollup.rs`, `src/infra/storage/query/rollup_tests.rs`

**Interfaces:**
- Consumes: Task 3's `HourSplit`; the existing `translate_scope`, `SqlCtx`, `SqlBind`, `ledger_from_clause` and `aggregate_limit_clause`.
- Produces:
  - `pub struct RollupStatement { pub sql: String, pub binds: Vec<SqlBind>, pub dim_count: usize }`
  - `pub fn build_rollup_aggregate_sql(gts_type_id: &MeterTypeId, split: HourSplit, fold: AggregationFold, filter: Option<&ast::Expr>, group_by: &[AggregationDimension]) -> Result<RollupStatement, String>`
  - **Precondition:** `rollup_eligible` returned `Ok(split)` for the same inputs. The builder re-checks only the fold, and returns `Err` for `Min`, `Max` or `Latest`.

- [ ] **Step 1: Write the failing tests.** Append to `rollup_tests.rs`, and extend the imports with `use usage_collector_sdk::MeterTypeId;`, `use super::build_rollup_aggregate_sql;` and `use crate::infra::storage::query::translate::SqlBind;`:

```rust
// ── build_rollup_aggregate_sql ─────────────────────────────────────────────

const VCPU_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";
const TYPE_KEY: &str = "r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = $1)";

fn meter() -> MeterTypeId {
    MeterTypeId::new(VCPU_METER).expect("valid meter")
}

fn two_tenants() -> ast::Expr {
    parsed(
        "tenant_id eq 11111111-1111-1111-1111-111111111111 \
         or tenant_id eq 22222222-2222-2222-2222-222222222222",
    )
}

/// `[22:13:20, 03:07:00)`: both edges, whole hours `[23:00, 03:00)`.
fn both_edges() -> HourSplit {
    hour_split(range(H0 + 800, H0 + 5 * HOUR + 420)).expect("whole hours")
}

#[test]
fn a_grouped_statement_with_both_edges_and_a_filter_reads_as_the_spec_states() {
    let st = build_rollup_aggregate_sql(
        &meter(),
        both_edges(),
        AggregationFold::Sum,
        Some(&two_tenants()),
        &[AggregationDimension::TenantId],
    )
    .expect("renders");

    assert_eq!(
        st.sql,
        format!(
            "WITH parts AS (\
             SELECT r.tenant_id::text AS d, r.sum_value AS s, r.count_value AS c \
             FROM usage_rollup_1h r \
             WHERE r.gts_type_id = $1 AND {TYPE_KEY} \
             AND r.bucket >= $2 AND r.bucket < $3 \
             AND ((tenant_id = $4 OR tenant_id = $5)) \
             UNION ALL \
             SELECT r.tenant_id::text AS d, \
             SUM(CASE WHEN r.invalidates IS NULL THEN r.value ELSE -r.value END)::numeric AS s, \
             SUM(CASE WHEN r.invalidates IS NULL THEN 1 ELSE -1 END)::bigint AS c \
             FROM usage_records r \
             WHERE r.gts_type_id = $1 AND {TYPE_KEY} \
             AND ((r.window_end >= $6 AND r.window_end < $7) OR (r.window_end >= $8 AND r.window_end < $9)) \
             AND ((tenant_id = $10 OR tenant_id = $11)) \
             GROUP BY 1) \
             SELECT d, SUM(s)::numeric FROM parts GROUP BY 1 HAVING SUM(c) <> 0 LIMIT 100001"
        )
    );
    assert_eq!(st.dim_count, 1);

    let split = both_edges();
    assert_eq!(st.binds.len(), 11);
    assert!(matches!(&st.binds[0], SqlBind::Str(s) if s == VCPU_METER));
    assert!(matches!(st.binds[1], SqlBind::DateTime(t) if t == split.whole_from));
    assert!(matches!(st.binds[2], SqlBind::DateTime(t) if t == split.whole_to));
    assert!(matches!(st.binds[3], SqlBind::Uuid(_)));
    assert!(matches!(st.binds[4], SqlBind::Uuid(_)));
    assert!(matches!(st.binds[5], SqlBind::DateTime(t) if t == split.from));
    assert!(matches!(st.binds[6], SqlBind::DateTime(t) if t == split.whole_from));
    assert!(matches!(st.binds[7], SqlBind::DateTime(t) if t == split.whole_to));
    assert!(matches!(st.binds[8], SqlBind::DateTime(t) if t == split.to));
    assert!(matches!(st.binds[9], SqlBind::Uuid(_)));
    assert!(matches!(st.binds[10], SqlBind::Uuid(_)));
}

#[test]
fn an_aligned_ungrouped_sum_reads_the_rollup_alone_and_nulls_a_netted_selection() {
    let split = hour_split(aligned()).expect("whole hours");
    let st = build_rollup_aggregate_sql(&meter(), split, AggregationFold::Sum, None, &[])
        .expect("renders");
    assert_eq!(
        st.sql,
        format!(
            "WITH parts AS (\
             SELECT r.sum_value AS s, r.count_value AS c \
             FROM usage_rollup_1h r \
             WHERE r.gts_type_id = $1 AND {TYPE_KEY} \
             AND r.bucket >= $2 AND r.bucket < $3) \
             SELECT (CASE WHEN COALESCE(SUM(c), 0) = 0 THEN NULL ELSE SUM(s) END)::numeric FROM parts"
        )
    );
    assert_eq!((st.binds.len(), st.dim_count), (3, 0));
}

#[test]
fn an_ungrouped_count_is_zero_rather_than_null_when_nothing_is_left() {
    let split = hour_split(aligned()).expect("whole hours");
    let st = build_rollup_aggregate_sql(&meter(), split, AggregationFold::Count, None, &[])
        .expect("renders");
    assert!(st.sql.ends_with("SELECT COALESCE(SUM(c), 0)::numeric FROM parts"), "{}", st.sql);
    assert!(!st.sql.contains("HAVING") && !st.sql.contains("LIMIT"), "{}", st.sql);
}

#[test]
fn a_grouped_count_sums_the_counts_and_drops_netted_groups() {
    let split = hour_split(aligned()).expect("whole hours");
    let st = build_rollup_aggregate_sql(
        &meter(),
        split,
        AggregationFold::Count,
        None,
        &[AggregationDimension::TenantId],
    )
    .expect("renders");
    assert!(
        st.sql.ends_with("SELECT d, SUM(c)::numeric FROM parts GROUP BY 1 HAVING SUM(c) <> 0 LIMIT 100001"),
        "{}",
        st.sql
    );
}

#[test]
fn a_single_edge_renders_one_disjunct() {
    let split = hour_split(range(H0, H0 + 2 * HOUR + 1)).expect("whole hours");
    let st = build_rollup_aggregate_sql(&meter(), split, AggregationFold::Sum, None, &[])
        .expect("renders");
    assert!(
        st.sql.contains("AND ((r.window_end >= $4 AND r.window_end < $5))"),
        "{}",
        st.sql
    );
    assert_eq!(st.binds.len(), 5);
}

#[test]
fn no_part_of_the_statement_selects_on_window_start() {
    let st = build_rollup_aggregate_sql(
        &meter(),
        both_edges(),
        AggregationFold::Sum,
        Some(&two_tenants()),
        &[AggregationDimension::TenantId],
    )
    .expect("renders");
    assert!(!st.sql.contains("window_start"), "{}", st.sql);
}

#[test]
fn a_fold_the_rollup_cannot_answer_is_refused() {
    let split = hour_split(aligned()).expect("whole hours");
    for fold in [AggregationFold::Min, AggregationFold::Max, AggregationFold::Latest] {
        assert!(build_rollup_aggregate_sql(&meter(), split, fold, None, &[]).is_err());
    }
}
```

- [ ] **Step 2: Run them and see them fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib rollup_tests`
Expected: compile error, because `build_rollup_aggregate_sql` does not exist yet.

- [ ] **Step 3: Implement** — append to `rollup.rs` (above the test-module wiring). Extend its imports: `use usage_collector_sdk::MeterTypeId;`, `use super::aggregate::aggregate_limit_clause;`, `use super::ledger_from_clause;` and `use super::translate::{SqlBind, SqlCtx, translate_scope};`.

```rust
/// The signed quantity of one ledger entry, as the rollup view sums it.
const SIGNED_VALUE: &str = "CASE WHEN r.invalidates IS NULL THEN r.value ELSE -r.value END";
/// The signed count of one ledger entry, as the rollup view sums it.
const SIGNED_COUNT: &str = "CASE WHEN r.invalidates IS NULL THEN 1 ELSE -1 END";

/// The meter and its partition-key subquery, both reading the meter's bind at
/// `$meter`. Both halves of the statement start with these, so chunk exclusion
/// applies to the view's materialised half and to the ledger edges alike.
fn meter_scope(meter: usize) -> Vec<String> {
    vec![
        format!("r.gts_type_id = ${meter}"),
        format!("r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = ${meter})"),
    ]
}

/// A rendered rollup aggregate: the statement, its binds in placeholder order,
/// and how many leading key columns each result row carries.
#[derive(Debug)]
pub struct RollupStatement {
    pub sql: String,
    pub binds: Vec<SqlBind>,
    pub dim_count: usize,
}

/// Render the aggregate for an eligible query: whole hours from
/// `usage_rollup_1h`, partial edge hours from the ledger, summed in one
/// statement.
///
/// The result shape matches the scan's exactly (spec §6.4). A grouped query
/// drops a group whose count nets to zero, because the scan never forms a group
/// from withdrawn entries alone. An ungrouped `SUM` is `NULL` over a selection
/// whose count nets to zero, and an ungrouped `COUNT` is `0`. Both tests read
/// the count, never the sum, so a genuine zero or negative total still returns.
///
/// Precondition: [`rollup_eligible`] returned `Ok(split)` for these inputs. The
/// filter is rendered once per half with its own placeholders. Its bare column
/// names are valid against both `FROM` clauses, since each half reads one
/// relation.
///
/// # Errors
///
/// Returns an error string for a fold other than `Sum` or `Count`, or when the
/// filter cannot be translated.
pub fn build_rollup_aggregate_sql(
    gts_type_id: &MeterTypeId,
    split: HourSplit,
    fold: AggregationFold,
    filter: Option<&ast::Expr>,
    group_by: &[AggregationDimension],
) -> Result<RollupStatement, String> {
    let grouped = !group_by.is_empty();
    let fold_expr = match (fold, grouped) {
        (AggregationFold::Sum, false) => {
            "(CASE WHEN COALESCE(SUM(c), 0) = 0 THEN NULL ELSE SUM(s) END)::numeric"
        }
        (AggregationFold::Count, false) => "COALESCE(SUM(c), 0)::numeric",
        (AggregationFold::Sum, true) => "SUM(s)::numeric",
        (AggregationFold::Count, true) => "SUM(c)::numeric",
        (other, _) => return Err(format!("the rollup does not serve the {other} fold")),
    };
    let dim = if grouped { "r.tenant_id::text AS d, " } else { "" };

    let mut ctx = SqlCtx::new(1);
    let meter = ctx.push(SqlBind::Str(gts_type_id.as_str().to_owned()));

    let mut rollup_where = meter_scope(meter);
    rollup_where.push(format!("r.bucket >= ${}", ctx.push(SqlBind::DateTime(split.whole_from))));
    rollup_where.push(format!("r.bucket < ${}", ctx.push(SqlBind::DateTime(split.whole_to))));
    if let Some(expr) = filter {
        rollup_where.push(translate_scope(expr, &mut ctx)?);
    }
    let mut parts = vec![format!(
        "SELECT {dim}r.sum_value AS s, r.count_value AS c FROM usage_rollup_1h r WHERE {}",
        rollup_where.join(" AND ")
    )];

    let mut edges = Vec::new();
    if split.has_lower_edge() {
        let a = ctx.push(SqlBind::DateTime(split.from));
        let b = ctx.push(SqlBind::DateTime(split.whole_from));
        edges.push(format!("(r.window_end >= ${a} AND r.window_end < ${b})"));
    }
    if split.has_upper_edge() {
        let a = ctx.push(SqlBind::DateTime(split.whole_to));
        let b = ctx.push(SqlBind::DateTime(split.to));
        edges.push(format!("(r.window_end >= ${a} AND r.window_end < ${b})"));
    }
    if !edges.is_empty() {
        let mut edge_where = meter_scope(meter);
        edge_where.push(format!("({})", edges.join(" OR ")));
        if let Some(expr) = filter {
            edge_where.push(translate_scope(expr, &mut ctx)?);
        }
        let group = if grouped { " GROUP BY 1" } else { "" };
        parts.push(format!(
            "SELECT {dim}SUM({SIGNED_VALUE})::numeric AS s, SUM({SIGNED_COUNT})::bigint AS c \
             FROM {} WHERE {}{group}",
            ledger_from_clause(),
            edge_where.join(" AND ")
        ));
    }

    let dim_count = usize::from(grouped);
    let (outer_dim, tail) = if grouped {
        (
            "d, ",
            format!(" GROUP BY 1 HAVING SUM(c) <> 0{}", aggregate_limit_clause(dim_count)),
        )
    } else {
        ("", String::new())
    };
    Ok(RollupStatement {
        sql: format!(
            "WITH parts AS ({}) SELECT {outer_dim}{fold_expr} FROM parts{tail}",
            parts.join(" UNION ALL ")
        ),
        binds: ctx.binds,
        dim_count,
    })
}
```

- [ ] **Step 4: Run the tests and see them pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib rollup_tests`
Expected: PASS. If a string assertion fails only on whitespace, fix the builder, not the expectation. The expectation is the spec's statement.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
P=gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src/infra/storage/query
git add $P/rollup.rs $P/rollup_tests.rs
git commit -m "feat(timescaledb-plugin): render SUM/COUNT from whole rollup hours plus exact ledger edges"
```

---

### Task 5: Route eligible aggregates to the rollup

**Files:**
- Modify: `src/infra/metrics.rs`, `src/infra/metrics_tests.rs`
- Modify: `src/infra/storage/record_store.rs`, `src/infra/storage/record_store_tests.rs`
- Create: `tests/rollup_aggregate_integration_pg.rs`

**Interfaces:**
- Consumes: Task 3's `rollup_eligible` and `FallbackReason`; Task 4's `build_rollup_aggregate_sql` and `RollupStatement`; Task 2's `common::refresh_rollup`.
- Produces:
  - `Metrics::record_aggregate_path(&self, fallback: Option<FallbackReason>)`: `None` means the rollup served the query.
  - `PgRecordStore::without_rollup(self) -> Self`, gated `#[cfg(any(test, feature = "postgres"))]`.

- [ ] **Step 1: Write the failing metrics test.** In `metrics_tests.rs`, add `use crate::infra::storage::query::rollup::FallbackReason;` and append:

```rust
#[tokio::test]
async fn the_aggregate_path_counter_splits_by_path_and_reason() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    metrics.record_aggregate_path(None);
    metrics.record_aggregate_path(None);
    metrics.record_aggregate_path(Some(FallbackReason::FilterField));
    provider.force_flush().unwrap();

    let name = "uc_timescaledb_aggregate_path_total";
    assert_eq!(counter_sum_with_label(&exporter, name, label::AGGREGATE_PATH, label::AGGREGATE_PATH_ROLLUP), 2);
    assert_eq!(counter_sum_with_label(&exporter, name, label::AGGREGATE_PATH, label::AGGREGATE_PATH_SCAN), 1);
    assert_eq!(counter_sum_with_label(&exporter, name, label::FALLBACK_REASON, "filter_field"), 1);
    assert_eq!(counter_sum_with_label(&exporter, name, label::FALLBACK_REASON, label::FALLBACK_REASON_NONE), 2);
}
```

In `every_exported_instrument_obeys_the_naming_convention`, add `metrics.record_aggregate_path(None);` after `metrics.set_chunks(1);`.

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib metrics_tests`
Expected: compile error.

- [ ] **Step 2: Implement the counter** in `metrics.rs`.

(a) Import `use crate::infra::storage::query::rollup::FallbackReason;`.

(b) Add to `mod label`:

```rust
    /// Label key for which read served an aggregate.
    pub const AGGREGATE_PATH: &str = "path";
    /// `path` value: whole hours from the rollup plus ledger edges.
    pub const AGGREGATE_PATH_ROLLUP: &str = "rollup";
    /// `path` value: the exact ledger scan.
    pub const AGGREGATE_PATH_SCAN: &str = "scan";
    /// Label key for why an aggregate took the scan.
    pub const FALLBACK_REASON: &str = "reason";
    /// `reason` value on `path="rollup"`.
    pub const FALLBACK_REASON_NONE: &str = "none";
```

(c) Add a field `/// \`uc_timescaledb_aggregate_path_total\` — labelled by \`path\` and \`reason\`.` `aggregate_path: Counter<u64>,` after `retention_drop_failures`. Build it in `with_meter`:

```rust
        let aggregate_path = meter
            .u64_counter("uc_timescaledb_aggregate_path_total")
            .with_description(
                "Aggregate queries by the read that served them: path=rollup (whole hours \
                 from usage_rollup_1h plus ledger edges) or path=scan, with the reason it fell back",
            )
            .build();
```

Add it to the `Self { … }` literal, to the `declared_instrument_names` destructure (`aggregate_path: _,`), and add `"uc_timescaledb_aggregate_path_total",` to its `vec!`.

(d) Add the helper:

```rust
    /// Count one aggregate by the read that served it: `None` for the rollup,
    /// `Some(reason)` for the scan.
    pub fn record_aggregate_path(&self, fallback: Option<FallbackReason>) {
        let (path, reason) = match fallback {
            None => (label::AGGREGATE_PATH_ROLLUP, label::FALLBACK_REASON_NONE),
            Some(r) => (label::AGGREGATE_PATH_SCAN, r.as_label()),
        };
        self.aggregate_path.add(
            1,
            &[
                KeyValue::new(label::AGGREGATE_PATH, path),
                KeyValue::new(label::FALLBACK_REASON, reason),
            ],
        );
    }
```

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib metrics_tests`
Expected: PASS.

- [ ] **Step 3: Write the failing unit test for the switch.** Append to `record_store_tests.rs`:

```rust
#[test]
fn a_store_serves_from_the_rollup_unless_switched_off() {
    assert!(lazy_store().rollup_enabled, "the rollup path is on by default");
    assert!(!lazy_store().without_rollup().rollup_enabled);
}
```

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib record_store_tests::a_store_serves`
Expected: compile error.

- [ ] **Step 4: Implement the routing** in `record_store.rs`.

(a) Add these imports:

```rust
use crate::infra::storage::query::rollup::{build_rollup_aggregate_sql, rollup_eligible};
```

(b) Add a field to `PgRecordStore`, and `rollup_enabled: true,` to `new`:

```rust
    /// Whether `aggregate` may serve an eligible query from `usage_rollup_1h`.
    /// Always `true` outside tests; [`Self::without_rollup`] turns it off so
    /// the integration suite can compare both reads over one database.
    rollup_enabled: bool,
```

(c) Add this after `new`:

```rust
    /// The same store with the rollup read switched off, so every aggregate
    /// takes the exact scan. Test-only: the equivalence suite compares the two.
    #[cfg(any(test, feature = "postgres"))]
    #[must_use]
    pub fn without_rollup(mut self) -> Self {
        self.rollup_enabled = false;
        self
    }
```

(d) In `aggregate`, replace the `let statement = build_aggregate_sql(…)…?;` block with:

```rust
        // The rollup answers an eligible SUM/COUNT exactly (spec §6); every
        // other query takes the scan it always has. The eligibility test reads
        // the same composed filter the scan would translate.
        let routed = if self.rollup_enabled {
            Some(rollup_eligible(fold, query.filter(), metadata_filter, group_by, time_range))
        } else {
            None
        };
        let statement = match routed {
            Some(Ok(split)) => {
                self.metrics.record_aggregate_path(None);
                let st = build_rollup_aggregate_sql(
                    &gts_type_id,
                    split,
                    fold,
                    query.filter(),
                    group_by,
                )
                .map_err(UsageCollectorPluginError::internal)?;
                AggregateStatement {
                    sql: st.sql,
                    binds: st.binds,
                    dim_count: st.dim_count,
                }
            }
            other => {
                if let Some(Err(reason)) = other {
                    self.metrics.record_aggregate_path(Some(reason));
                }
                build_aggregate_sql(
                    &gts_type_id,
                    time_range,
                    fold,
                    query,
                    metadata_filter,
                    group_by,
                )
                .map_err(UsageCollectorPluginError::internal)?
            }
        };
```

The rest of `aggregate` (acquire, fetch, `aggregate_bucket`) is unchanged, and so is the pinned unit test `the_ungrouped_fold_still_reaches_the_pool`: `list_range()` is 24 aligned hours, so an ungrouped `SUM` over it now takes the rollup path, and it still reaches the pool. Add one sentence to `aggregate`'s rustdoc: "An eligible `SUM` or `COUNT` is served from `usage_rollup_1h` (see `query::rollup`); the result is identical to the scan's."

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS.

- [ ] **Step 5: Write the integration suite.** Create `tests/rollup_aggregate_integration_pg.rs`:

```rust
#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The rollup read path against a live `TimescaleDB`, compared with the exact
//! scan over the same database. Requires Docker.
//!
//! The equivalence test is what holds the rollup to the contract's rules: the
//! SDK contract checks use sub-hour ranges, which always take the scan.

mod common;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use toolkit_odata::ODataQuery;
use toolkit_odata::ast::{CompareOperator, Expr, Value};
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, AggregationResult, RecordOrigin, TimeRange, UsageRecord,
};

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

const TENANT_A: Uuid = Uuid::from_u128(0xA0);
const TENANT_B: Uuid = Uuid::from_u128(0xB0);
/// Every entry of this tenant is withdrawn.
const TENANT_C: Uuid = Uuid::from_u128(0xC0);

/// `2023-11-14T22:00:00Z`.
fn h0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_699_999_200).expect("valid instant")
}

fn tenant_eq(t: Uuid) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier("tenant_id".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::Uuid(t))),
    )
}

fn origin_eq(origin: &str) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier("origin".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::String(origin.to_owned()))),
    )
}

fn range(from: OffsetDateTime, to: OffsetDateTime) -> TimeRange {
    TimeRange::new(from, to).expect("ordered range")
}

/// Buckets as sorted `(key, normalized value)` pairs, so two results compare
/// as sets whatever order `PostgreSQL` emitted them in.
fn canonical(result: &AggregationResult) -> Vec<(Vec<String>, Option<BigDecimal>)> {
    let mut rows: Vec<_> = result
        .buckets
        .iter()
        .map(|b| (b.key.clone(), b.value.as_ref().map(BigDecimal::normalized)))
        .collect();
    rows.sort();
    rows
}

struct Stores {
    h: common::TsHarness,
    rollup: PgRecordStore,
    scan: PgRecordStore,
}

async fn stores() -> Stores {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let rollup = common::record_store(&h.pool);
    let scan = common::record_store(&h.pool).without_rollup();
    Stores { h, rollup, scan }
}

/// Deterministic fixture: two meters, three tenants, 30 hours of entries at
/// minute offsets that include exact hour boundaries and point events,
/// negative quantities, withdrawals (one of them on the backfill path of a live
/// record), and a tenant whose every entry is withdrawn.
async fn seed(store: &PgRecordStore) {
    let meters = [common::meter(common::VCPU_METER), common::meter(common::GB_METER)];
    let mut n = 0_u32;
    for meter in &meters {
        for tenant in [TENANT_A, TENANT_B, TENANT_C] {
            for hour in 0_i64..30 {
                for minute in [0_i64, 17, 59] {
                    n += 1;
                    let end = h0() + Duration::hours(hour) + Duration::minutes(minute);
                    let start = if minute == 17 { end } else { end - Duration::minutes(45) };
                    let value = Decimal::from(i64::from(n % 7) - 2);
                    let record = common::entry_over(meter, tenant, &format!("e-{n}"), value, start, end);
                    let withdraw = tenant == TENANT_C || n % 5 == 0;
                    store.create(record.clone()).await.expect("seed record");
                    if withdraw {
                        let mut w = common::withdrawal_of(&record, &format!("w-{n}"));
                        if n % 10 == 0 {
                            w.origin = RecordOrigin::Backfill;
                        }
                        store.create(w).await.expect("seed withdrawal");
                    }
                }
            }
        }
    }
}

fn ranges() -> Vec<TimeRange> {
    vec![
        range(h0(), h0() + Duration::hours(24)),
        range(h0() + Duration::minutes(13), h0() + Duration::hours(5) + Duration::minutes(7)),
        range(h0() + Duration::minutes(10), h0() + Duration::minutes(50)),
        range(h0() - Duration::hours(1), h0() + Duration::hours(40)),
        range(h0() + Duration::hours(1), h0() + Duration::hours(2)),
        range(h0() + Duration::hours(3) + Duration::minutes(17), h0() + Duration::hours(7) + Duration::minutes(59)),
    ]
}

/// Every (range, fold, grouping, filter) on both stores, asserting equality.
async fn assert_paths_agree(s: &Stores, when: &str) {
    let filters: Vec<Option<Expr>> = vec![
        None,
        Some(tenant_eq(TENANT_A)),
        Some(Expr::Or(Box::new(tenant_eq(TENANT_A)), Box::new(tenant_eq(TENANT_C)))),
        Some(tenant_eq(TENANT_C)),
    ];
    for meter in [common::VCPU_METER, common::GB_METER] {
        for r in ranges() {
            for fold in [AggregationFold::Sum, AggregationFold::Count] {
                for group_by in [vec![], vec![AggregationDimension::TenantId]] {
                    for f in &filters {
                        let q = f.clone().map_or_else(ODataQuery::new, |e| ODataQuery::new().with_filter(e));
                        let got = s.rollup.aggregate(common::meter(meter), r, fold, &q, &[], &group_by).await.expect("rollup");
                        let want = s.scan.aggregate(common::meter(meter), r, fold, &q, &[], &group_by).await.expect("scan");
                        assert_eq!(
                            canonical(&got),
                            canonical(&want),
                            "{when}: {meter} {r:?} {fold:?} {group_by:?} {f:?}"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rollup_path_equals_the_scan_before_and_after_a_refresh() {
    let s = stores().await;
    seed(&s.rollup).await;
    assert_paths_agree(&s, "before refresh (real-time half only)").await;
    common::refresh_rollup(&s.h.pool).await;
    assert_paths_agree(&s, "after refresh").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fully_withdrawn_tenant_has_no_group_and_folds_to_null_sum_and_zero_count() {
    let s = stores().await;
    seed(&s.rollup).await;
    common::refresh_rollup(&s.h.pool).await;
    let meter = common::meter(common::VCPU_METER);
    let day = range(h0(), h0() + Duration::hours(24));

    let grouped = s
        .rollup
        .aggregate(meter.clone(), day, AggregationFold::Sum, &ODataQuery::new(), &[], &[AggregationDimension::TenantId])
        .await
        .expect("grouped");
    assert!(
        grouped.buckets.iter().all(|b| b.key != vec![TENANT_C.to_string()]),
        "{grouped:?}"
    );

    let only_c = ODataQuery::new().with_filter(tenant_eq(TENANT_C));
    let sum = s.rollup.aggregate(meter.clone(), day, AggregationFold::Sum, &only_c, &[], &[]).await.expect("sum");
    assert_eq!(sum.buckets.len(), 1);
    assert_eq!(sum.buckets[0].value, None, "a SUM over nothing is null");
    let count = s.rollup.aggregate(meter, day, AggregationFold::Count, &only_c, &[], &[]).await.expect("count");
    assert_eq!(count.buckets[0].value.as_ref().map(BigDecimal::normalized), Some(BigDecimal::from(0).normalized()));
}

/// Routing by behaviour: after a refresh, a late write below the watermark is
/// invisible to an eligible query until the next refresh, and visible at once
/// to the same query with an `origin` filter, which takes the scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_write_is_stale_on_the_rollup_until_refreshed_and_fresh_on_the_scan() {
    let s = stores().await;
    seed(&s.rollup).await;
    common::refresh_rollup(&s.h.pool).await;
    let meter = common::meter(common::VCPU_METER);
    let day = range(h0(), h0() + Duration::hours(24));
    let a = ODataQuery::new().with_filter(tenant_eq(TENANT_A));
    let a_live = ODataQuery::new().with_filter(Expr::And(Box::new(tenant_eq(TENANT_A)), Box::new(origin_eq("live"))));
    let sum = |r: &AggregationResult| r.buckets[0].value.clone().map(|v| v.normalized());

    let before = sum(&s.rollup.aggregate(meter.clone(), day, AggregationFold::Sum, &a, &[], &[]).await.unwrap());
    let before_live = sum(&s.rollup.aggregate(meter.clone(), day, AggregationFold::Sum, &a_live, &[], &[]).await.unwrap());

    let late_end = h0() + Duration::hours(6) + Duration::minutes(30);
    let late: UsageRecord = common::entry_over(&meter, TENANT_A, "late", Decimal::from(1000), late_end - Duration::minutes(5), late_end);
    s.rollup.create(late).await.expect("late write");

    let stale = sum(&s.rollup.aggregate(meter.clone(), day, AggregationFold::Sum, &a, &[], &[]).await.unwrap());
    assert_eq!(stale, before, "the rollup path does not see a write below its watermark before a refresh");
    let fresh_live = sum(&s.rollup.aggregate(meter.clone(), day, AggregationFold::Sum, &a_live, &[], &[]).await.unwrap());
    assert_eq!(
        fresh_live,
        before_live.map(|v| (v + BigDecimal::from(1000)).normalized()),
        "an origin filter takes the scan, which sees the write at once"
    );

    common::refresh_rollup(&s.h.pool).await;
    let refreshed = sum(&s.rollup.aggregate(meter, day, AggregationFold::Sum, &a, &[], &[]).await.unwrap());
    assert_eq!(refreshed, before.map(|v| (v + BigDecimal::from(1000)).normalized()));
}

/// A write inside the materialisation lag is served by the view's real-time
/// half without any refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recent_write_is_visible_on_the_rollup_path_without_a_refresh() {
    let s = stores().await;
    seed(&s.rollup).await;
    common::refresh_rollup(&s.h.pool).await;
    let meter = common::meter(common::VCPU_METER);
    let end = OffsetDateTime::now_utc() - Duration::minutes(10);
    s.rollup
        .create(common::entry_over(&meter, TENANT_B, "recent", Decimal::from(42), end - Duration::minutes(5), end))
        .await
        .expect("recent write");
    let recent = range(end - Duration::hours(3), end + Duration::hours(2));
    let q = ODataQuery::new().with_filter(tenant_eq(TENANT_B));
    let got = s.rollup.aggregate(meter, recent, AggregationFold::Sum, &q, &[], &[]).await.expect("aggregate");
    assert_eq!(
        got.buckets[0].value.as_ref().map(BigDecimal::normalized),
        Some(BigDecimal::from(42).normalized())
    );
}
```

Two checks while writing it:
- `toolkit_odata::ast::Expr` has `And(Box<Expr>, Box<Expr>)` and `Or(Box<Expr>, Box<Expr>)`, as used above.
- `common::entry_over` fixes `resource_ref` and `metadata`, so `withdrawal_of` copies them verbatim.

- [ ] **Step 6: Run the suite**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test rollup_aggregate_integration_pg --test records_query_integration_pg --test contract_conformance_pg`
Expected: PASS. If equivalence fails, the assertion message names the query. Fix the builder, never the expectation.

- [ ] **Step 7: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
P=gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git add $P/src/infra/metrics.rs $P/src/infra/metrics_tests.rs \
        $P/src/infra/storage/record_store.rs $P/src/infra/storage/record_store_tests.rs \
        $P/tests/rollup_aggregate_integration_pg.rs
git commit -m "feat(timescaledb-plugin): serve eligible SUM/COUNT aggregates from the hourly rollup"
```

---

### Task 6: Cut the rollup with every retention drop

**Files:**
- Modify: `src/infra/storage/rollup_maintenance.rs`, `src/infra/storage/rollup_maintenance_tests.rs`
- Modify: `src/infra/storage/retention_sweep.rs`, `src/infra/storage/retention_sweep_tests.rs`
- Modify: `src/infra/metrics.rs`, `src/infra/metrics_tests.rs`
- Modify: `tests/retention_sweep_integration_pg.rs`, `tests/schema_integration_pg.rs`

**Interfaces:**
- Consumes: Task 2's `rollup_maintenance` module and `common::refresh_rollup`.
- Produces:
  - `pub const MATERIALIZATION_TABLE_SQL: &str`, `pub async fn materialization_table(pool: &PgPool) -> Result<Option<String>, sqlx::Error>`, `pub fn delete_rollup_rows_sql(table: &str) -> String` in `rollup_maintenance.rs`.
  - `ChunkSlice.time_start: OffsetDateTime`
  - `SweepReport.rollup_rows_deleted: u64`
  - `Metrics::add_rollup_rows_deleted(&self, n: u64)`

- [ ] **Step 1: Write the failing unit tests.**

Append to `rollup_maintenance_tests.rs`, extending its `use super::{…}` with `MATERIALIZATION_TABLE_SQL, delete_rollup_rows_sql`:

```rust
#[test]
fn the_materialisation_table_is_read_from_the_public_information_view() {
    assert_eq!(
        MATERIALIZATION_TABLE_SQL,
        "SELECT format('%I.%I', materialization_hypertable_schema, materialization_hypertable_name) \
         FROM timescaledb_information.continuous_aggregates \
         WHERE view_schema = current_schema() AND view_name = 'usage_rollup_1h'"
    );
}

#[test]
fn the_rollup_cut_removes_only_buckets_wholly_inside_the_chunk_and_its_key_range() {
    assert_eq!(
        delete_rollup_rows_sql("_timescaledb_internal._materialized_hypertable_2"),
        "DELETE FROM _timescaledb_internal._materialized_hypertable_2 \
         WHERE type_key >= $1::bigint AND type_key < $2::bigint \
         AND bucket >= $3 AND bucket + INTERVAL '1 hour' <= $4"
    );
}
```

Append to `retention_sweep_tests.rs`:

```rust
#[test]
fn the_catalog_query_reads_the_time_range_start_too() {
    assert!(LIST_CHUNKS_SQL.contains(
        "_timescaledb_functions.to_timestamp(\
         max(ds.range_start) FILTER (WHERE d.column_name = 'window_end')) AS time_start"
    ));
}
```

In `metrics_tests.rs::every_exported_instrument_obeys_the_naming_convention`, add `metrics.add_rollup_rows_deleted(3);`.

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: compile errors.

- [ ] **Step 2: Implement the lookup and the cut SQL** — append to `rollup_maintenance.rs`, above the test wiring:

```rust
/// The rollup's materialisation hypertable, schema-qualified and quoted with
/// `%I` by the database, from the public information view rather than the
/// internal catalog.
pub const MATERIALIZATION_TABLE_SQL: &str = "SELECT format('%I.%I', materialization_hypertable_schema, materialization_hypertable_name) \
     FROM timescaledb_information.continuous_aggregates \
     WHERE view_schema = current_schema() AND view_name = 'usage_rollup_1h'";

/// The materialisation hypertable, or `None` when the rollup does not exist.
///
/// # Errors
/// Returns `sqlx::Error` if the query fails.
pub async fn materialization_table(pool: &PgPool) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(MATERIALIZATION_TABLE_SQL)
        .fetch_optional(pool)
        .await
}

/// Deletes the rollup rows a dropped ledger chunk fed: `$1`/`$2` the chunk's
/// `type_key` range, `$3`/`$4` its `window_end` range.
///
/// Only buckets **wholly** inside the chunk are cut. With the hour-multiple
/// chunk interval every bucket is; a straddling bucket from a chunk created
/// under an earlier interval is kept while its neighbour still holds rows, so
/// the rollup can over-retain one bucket and never under-count.
///
/// `table` must come from [`materialization_table`], which quotes it; it is
/// never caller input.
#[must_use]
pub fn delete_rollup_rows_sql(table: &str) -> String {
    format!(
        "DELETE FROM {table} WHERE type_key >= $1::bigint AND type_key < $2::bigint \
         AND bucket >= $3 AND bucket + INTERVAL '1 hour' <= $4"
    )
}
```

- [ ] **Step 3: Add the counter** in `metrics.rs`.
  - Add a field: `/// \`uc_timescaledb_rollup_rows_deleted_total\`.` `rollup_rows_deleted: Counter<u64>,`.
  - Build it with `.u64_counter("uc_timescaledb_rollup_rows_deleted_total").with_description("Rollup rows deleted with the ledger chunks the retention sweep dropped").build()`.
  - Add it to `Self { … }`, to the destructure, and to the `vec!`.
  - Add the helper:

```rust
    /// Add `n` to the deleted-rollup-rows counter.
    pub fn add_rollup_rows_deleted(&self, n: u64) {
        self.rollup_rows_deleted.add(n, &[]);
    }
```

- [ ] **Step 4: Couple the sweep** in `retention_sweep.rs`.

(a) Add these imports:

```rust
use sqlx::AssertSqlSafe;

use crate::infra::storage::rollup_maintenance::{delete_rollup_rows_sql, materialization_table};
```

(b) Add `time_start` to `LIST_CHUNKS_SQL`, directly after `SELECT ch.relid::text AS chunk, `:

```rust
pub const LIST_CHUNKS_SQL: &str = "SELECT ch.relid::text AS chunk, \
     _timescaledb_functions.to_timestamp(\
     max(ds.range_start) FILTER (WHERE d.column_name = 'window_end')) AS time_start, \
     _timescaledb_functions.to_timestamp(\
     max(ds.range_end) FILTER (WHERE d.column_name = 'window_end')) AS time_end, \
     max(ds.range_start) FILTER (WHERE d.column_name = 'type_key') AS key_start, \
     max(ds.range_end) FILTER (WHERE d.column_name = 'type_key') AS key_end \
     FROM _timescaledb_catalog.chunk ch \
     JOIN _timescaledb_catalog.hypertable h ON h.id = ch.hypertable_id \
     JOIN _timescaledb_catalog.dimension_slice ds ON ds.chunk_id = ch.id \
     JOIN _timescaledb_catalog.dimension d ON d.id = ds.dimension_id \
     WHERE h.table_name = 'usage_records' AND h.schema_name = current_schema() \
     GROUP BY ch.relid";
```

(c) Add the field to `ChunkSlice`, before `time_end`:

```rust
    /// Inclusive start of the chunk's `window_end` range.
    pub time_start: OffsetDateTime,
```

(d) Widen `ChunkRow` to five fields and destructure them in `list_chunks`:

```rust
type ChunkRow = (
    String,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    Option<i64>,
    Option<i64>,
);
```

```rust
        .filter_map(|(chunk, time_start, time_end, key_start, key_end)| {
            let (Some(time_start), Some(time_end), Some(key_start), Some(key_end)) =
                (time_start, time_end, key_start, key_end)
            else {
```

Then add `time_start,` to the `ChunkSlice { … }` literal. Update the constant's doc first line to "Every chunk of `usage_records` with its time range and type-key range."

(e) Add `pub rollup_rows_deleted: u64,` to `SweepReport`.

(f) In `sweep`, resolve the table right after `list_chunks`, before any decision:

```rust
        // A ledger drop must take its rollup rows with it, so without the
        // rollup's table nothing is dropped this sweep.
        let Some(rollup_table) = materialization_table(&self.pool).await? else {
            tracing::warn!("the rollup's materialisation table is missing; this sweep drops nothing");
            return Err(sqlx::Error::RowNotFound);
        };
```

Pass `&rollup_table` through `apply_decision(chunk, decision, &rollup_table, &mut report)` to `drop_chunk`.

(g) Replace `drop_chunk` with:

```rust
    /// Drop one expired chunk and its rollup rows, counting the outcome either
    /// way.
    async fn drop_chunk(&self, chunk: &ChunkSlice, rollup_table: &str, report: &mut SweepReport) {
        match self.drop_chunk_and_rollup_rows(chunk, rollup_table).await {
            Ok(deleted) => {
                report.dropped += 1;
                report.rollup_rows_deleted += deleted;
                self.metrics.inc_retention_chunk_dropped();
                self.metrics.add_rollup_rows_deleted(deleted);
            }
            Err(e) => {
                report.drop_failures += 1;
                self.metrics.inc_retention_drop_failure();
                tracing::warn!(
                    chunk = %chunk.chunk,
                    error = %e,
                    "dropping an expired ledger chunk and its rollup rows failed; the next sweep retries it"
                );
            }
        }
    }

    /// The chunk drop and the rollup cut, atomically: a rollup row never
    /// outlives the ledger rows it was computed from, and a failed cut keeps
    /// the chunk. Returns the rollup rows deleted.
    async fn drop_chunk_and_rollup_rows(
        &self,
        chunk: &ChunkSlice,
        rollup_table: &str,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(DROP_CHUNK_SQL)
            .bind(&chunk.chunk)
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query(AssertSqlSafe(delete_rollup_rows_sql(rollup_table)))
            .bind(chunk.key_start)
            .bind(chunk.key_end)
            .bind(chunk.time_start)
            .bind(chunk.time_end)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(deleted)
    }
```

Extend `sweep_once`'s `# Errors` doc: "…loading type keys, or resolving the rollup's materialisation table (a missing rollup ends the sweep before any drop)".

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS.

- [ ] **Step 5: Write the integration tests.** Append to `tests/retention_sweep_integration_pg.rs`:

```rust
async fn rollup_rows(pool: &sqlx::PgPool, meter: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM usage_rollup_1h WHERE gts_type_id = $1")
        .bind(meter)
        .fetch_one(pool)
        .await
        .expect("count rollup rows")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drop_takes_its_types_rollup_rows_and_leaves_the_others() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E10);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    stub.set(common::GB_METER, days(400));
    store.create(aged(common::VCPU_METER, tenant, "vcpu", 100)).await.expect("vcpu");
    store.create(aged(common::GB_METER, tenant, "gb", 100)).await.expect("gb");
    common::refresh_rollup(&h.pool).await;
    assert_eq!((rollup_rows(&h.pool, common::VCPU_METER).await, rollup_rows(&h.pool, common::GB_METER).await), (1, 1));

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!((report.dropped, report.rollup_rows_deleted), (1, 1), "{report:?}");
    assert_eq!(rollup_rows(&h.pool, common::VCPU_METER).await, 0, "the expired type's rollup rows go with its chunk");
    assert_eq!(rollup_rows(&h.pool, common::GB_METER).await, 1, "the other type's rollup rows stay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_slice_drop_cuts_every_type_in_its_key_range() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let widened = timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig {
        type_key_slice_width: 4,
        ..h.cfg.clone()
    };
    apply_post_migration_setup(&h.pool, &widened).await.expect("widen");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E11);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    stub.set(common::GB_METER, days(30));
    store.create(aged(common::VCPU_METER, tenant, "vcpu", 100)).await.expect("vcpu");
    store.create(aged(common::GB_METER, tenant, "gb", 100)).await.expect("gb");
    common::refresh_rollup(&h.pool).await;

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!((report.dropped, report.rollup_rows_deleted), (1, 2), "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_rollup_nothing_is_dropped() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    let entry = aged(common::VCPU_METER, Uuid::from_u128(0x5E12), "kept", 100);
    let id = entry.id;
    store.create(entry).await.expect("create");
    sqlx::query("DROP MATERIALIZED VIEW usage_rollup_1h")
        .execute(&h.pool)
        .await
        .expect("drop the rollup");

    assert!(sweeper(&h.pool, &stub).sweep_once().await.is_err());
    assert!(stored(&h.pool, id).await, "no rollup to cut, so no chunk is dropped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_write_into_a_dropped_range_is_the_only_thing_the_next_refresh_rolls_up() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E13);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    let first = aged(common::VCPU_METER, tenant, "first", 100);
    let (start, end) = (first.window_start, first.window_end);
    store.create(first).await.expect("first");
    common::refresh_rollup(&h.pool).await;
    assert_eq!(sweeper(&h.pool, &stub).sweep_once().await.expect("sweep").dropped, 1);

    let late = common::entry_over(
        &common::meter(common::VCPU_METER),
        tenant,
        "late",
        Decimal::from(3),
        start,
        end,
    );
    store.create(late).await.expect("late write recreates the chunk");
    common::refresh_rollup(&h.pool).await;

    let total: Option<rust_decimal::Decimal> =
        sqlx::query_scalar("SELECT sum(sum_value) FROM usage_rollup_1h WHERE gts_type_id = $1")
            .bind(common::VCPU_METER)
            .fetch_one(&h.pool)
            .await
            .expect("rollup total");
    assert_eq!(total, Some(Decimal::from(3)), "only the surviving late row is rolled up");
}
```

In `tests/schema_integration_pg.rs`, extend `the_chunk_catalog_query_reads_one_row_per_chunk_with_both_ranges`'s loop with:

```rust
        assert!(
            chunk.time_start < chunk.time_end && chunk.time_start <= common::fixture_window_end(),
            "the time range start bounds every window_end from below: {chunk:?}"
        );
```

Then append this test:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rollups_materialisation_table_resolves_to_a_hypertable() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let table = timescaledb_usage_collector_plugin::infra::storage::rollup_maintenance::materialization_table(&h.pool)
        .await
        .expect("lookup runs")
        .expect("the rollup exists");
    let is_hypertable: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM timescaledb_information.hypertables \
         WHERE format('%I.%I', hypertable_schema, hypertable_name) = $1)",
    )
    .bind(&table)
    .fetch_one(&h.pool)
    .await
    .expect("hypertable lookup");
    assert!(is_hypertable, "{table} must be the rollup's materialisation hypertable");
}
```

- [ ] **Step 6: Run the suites**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test retention_sweep_integration_pg --test schema_integration_pg`
Expected: PASS, including the seven existing sweep tests.

- [ ] **Step 7: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
P=gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git add $P/src/infra/storage/rollup_maintenance.rs $P/src/infra/storage/rollup_maintenance_tests.rs \
        $P/src/infra/storage/retention_sweep.rs $P/src/infra/storage/retention_sweep_tests.rs \
        $P/src/infra/metrics.rs $P/src/infra/metrics_tests.rs \
        $P/tests/retention_sweep_integration_pg.rs $P/tests/schema_integration_pg.rs
git commit -m "feat(timescaledb-plugin): delete a dropped chunk's rollup rows in the same transaction"
```

---

### Task 7: Publish refresh-policy health

**Files:**
- Modify: `src/infra/storage/rollup_maintenance.rs`, `src/infra/storage/rollup_maintenance_tests.rs`
- Modify: `src/infra/metrics.rs`, `src/infra/metrics_tests.rs`
- Modify: `src/gear.rs`
- Modify: `tests/rollup_aggregate_integration_pg.rs`

**Interfaces:**
- Consumes: Task 2's `rollup_maintenance` and `apply_post_migration_setup(pool, &cfg)`.
- Produces:
  - `pub enum RefreshPolicy { Live, History }` with `pub const fn as_label(self) -> &'static str`
  - `pub struct RefreshJobStatus { pub policy: RefreshPolicy, pub failing: bool, pub secs_since_success: Option<f64> }`
  - `pub fn job_status_from_row(is_history: bool, last_run_status: Option<&str>, age_secs: Option<f64>) -> RefreshJobStatus`
  - `pub async fn refresh_job_statuses(pool: &PgPool) -> Result<Vec<RefreshJobStatus>, sqlx::Error>`
  - `pub struct RollupMonitor` with `pub fn new(pool: PgPool, metrics: Arc<Metrics>) -> Self` and `pub async fn sample_once(&self) -> Result<usize, sqlx::Error>`
  - `Metrics::set_rollup_refresh_status(&self, status: &RefreshJobStatus)`

- [ ] **Step 1: Write the failing unit tests.**

Append to `rollup_maintenance_tests.rs`, extending the imports with `RefreshPolicy, job_status_from_row`:

```rust
#[test]
fn a_policy_with_no_start_offset_is_the_history_policy() {
    assert_eq!(job_status_from_row(true, Some("Success"), Some(5.0)).policy, RefreshPolicy::History);
    assert_eq!(job_status_from_row(false, Some("Success"), Some(5.0)).policy, RefreshPolicy::Live);
}

#[test]
fn only_a_failed_last_run_is_failing() {
    assert!(job_status_from_row(false, Some("Failure"), Some(5.0)).failing);
    assert!(!job_status_from_row(false, Some("Success"), Some(5.0)).failing);
    assert!(!job_status_from_row(false, None, None).failing, "a policy that never ran has not failed");
}

#[test]
fn a_policy_that_never_succeeded_reports_no_age() {
    // `last_successful_finish` is -infinity until the first success, so the
    // age reads back as +infinity.
    assert_eq!(job_status_from_row(false, None, Some(f64::INFINITY)).secs_since_success, None);
    assert_eq!(job_status_from_row(false, None, None).secs_since_success, None);
    assert_eq!(job_status_from_row(false, Some("Success"), Some(-1.0)).secs_since_success, Some(0.0));
    assert_eq!(job_status_from_row(false, Some("Success"), Some(42.5)).secs_since_success, Some(42.5));
}

#[test]
fn the_policy_labels_are_live_and_history() {
    assert_eq!(
        [RefreshPolicy::Live.as_label(), RefreshPolicy::History.as_label()],
        ["live", "history"]
    );
}
```

In `metrics_tests.rs`, add `use crate::infra::storage::rollup_maintenance::{RefreshJobStatus, RefreshPolicy};` and append:

```rust
#[tokio::test]
async fn refresh_status_sets_both_gauges_under_the_policy_label() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());
    metrics.set_rollup_refresh_status(&RefreshJobStatus {
        policy: RefreshPolicy::Live,
        failing: true,
        secs_since_success: Some(90.0),
    });
    provider.force_flush().unwrap();
    assert_eq!(gauge_last_u64(&exporter, "uc_timescaledb_rollup_refresh_job_failing"), Some(1));
    assert!(exported_names(&exporter).contains(&"uc_timescaledb_rollup_refresh_age_seconds".to_owned()));
}
```

In `every_exported_instrument_obeys_the_naming_convention`, add:

```rust
    metrics.set_rollup_refresh_status(&RefreshJobStatus {
        policy: RefreshPolicy::History,
        failing: false,
        secs_since_success: Some(1.0),
    });
```

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: compile errors.

- [ ] **Step 2: Implement the sampling** — append to `rollup_maintenance.rs`, above the test wiring. Add `use std::sync::Arc;` and `use crate::infra::metrics::Metrics;`.

```rust
/// Each rollup refresh policy with its last run status and the seconds since
/// its last success. `last_successful_finish` is `-infinity` before the first
/// success, which makes the age `+infinity`.
pub const REFRESH_JOB_STATUS_SQL: &str = "SELECT (j.config->>'start_offset') IS NULL AS is_history, \
     js.last_run_status::text, \
     EXTRACT(EPOCH FROM (now() - js.last_successful_finish))::double precision AS age_secs \
     FROM timescaledb_information.jobs j \
     LEFT JOIN timescaledb_information.job_stats js ON js.job_id = j.job_id \
     WHERE j.proc_name = 'policy_refresh_continuous_aggregate' \
     AND j.hypertable_schema = current_schema() AND j.hypertable_name = 'usage_rollup_1h'";

/// Which of the two refresh policies a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPolicy {
    /// The frequent policy over the live window.
    Live,
    /// The policy over everything older than the live window.
    History,
}

impl RefreshPolicy {
    /// The bounded `policy` label value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::History => "history",
        }
    }
}

/// One refresh policy's health.
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshJobStatus {
    pub policy: RefreshPolicy,
    /// The last run failed.
    pub failing: bool,
    /// Seconds since the last success; `None` if it has never succeeded.
    pub secs_since_success: Option<f64>,
}

/// Interpret one row of [`REFRESH_JOB_STATUS_SQL`].
#[must_use]
pub fn job_status_from_row(
    is_history: bool,
    last_run_status: Option<&str>,
    age_secs: Option<f64>,
) -> RefreshJobStatus {
    RefreshJobStatus {
        policy: if is_history { RefreshPolicy::History } else { RefreshPolicy::Live },
        failing: last_run_status == Some("Failure"),
        secs_since_success: age_secs.filter(|a| a.is_finite()).map(|a| a.max(0.0)),
    }
}

/// Every refresh policy of the rollup, as currently recorded.
///
/// # Errors
/// Returns `sqlx::Error` if the query fails.
pub async fn refresh_job_statuses(pool: &PgPool) -> Result<Vec<RefreshJobStatus>, sqlx::Error> {
    let rows: Vec<(bool, Option<String>, Option<f64>)> =
        sqlx::query_as(REFRESH_JOB_STATUS_SQL).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(is_history, status, age)| job_status_from_row(is_history, status.as_deref(), age))
        .collect())
}

/// Publishes refresh-policy health on the plugin's metric inventory.
pub struct RollupMonitor {
    pool: PgPool,
    metrics: Arc<Metrics>,
}

impl RollupMonitor {
    #[must_use]
    pub fn new(pool: PgPool, metrics: Arc<Metrics>) -> Self {
        Self { pool, metrics }
    }

    /// Read every policy's status and set its gauges. Returns how many
    /// policies were sampled.
    ///
    /// # Errors
    /// Returns `sqlx::Error` if the status query fails; the gauges then keep
    /// their last values.
    pub async fn sample_once(&self) -> Result<usize, sqlx::Error> {
        let statuses = refresh_job_statuses(&self.pool).await?;
        for status in &statuses {
            self.metrics.set_rollup_refresh_status(status);
        }
        Ok(statuses.len())
    }
}
```

- [ ] **Step 3: Add the gauges** in `metrics.rs`.

(a) Import `use crate::infra::storage::rollup_maintenance::RefreshJobStatus;`, and add `pub const REFRESH_POLICY: &str = "policy";` to `mod label`.

(b) Add these fields after `chunks`:

```rust
    /// `uc_timescaledb_rollup_refresh_age_seconds` — labelled by `policy`.
    rollup_refresh_age: Gauge<f64>,
    /// `uc_timescaledb_rollup_refresh_job_failing` — labelled by `policy`.
    rollup_refresh_job_failing: Gauge<u64>,
```

(c) Build them in `with_meter`:

```rust
        let rollup_refresh_age = meter
            .f64_gauge("uc_timescaledb_rollup_refresh_age_seconds")
            .with_description(
                "Seconds since each rollup refresh policy last succeeded, by policy; unset \
                 until its first success",
            )
            .build();
        let rollup_refresh_job_failing = meter
            .u64_gauge("uc_timescaledb_rollup_refresh_job_failing")
            .with_description("1 when a rollup refresh policy's last run failed, by policy")
            .build();
```

Add both to `Self { … }`, to the destructure and to the `vec!`: `"uc_timescaledb_rollup_refresh_age_seconds"` and `"uc_timescaledb_rollup_refresh_job_failing"`.

(d) Add the helper:

```rust
    /// Set one refresh policy's gauges. The age is left unset for a policy
    /// that has never succeeded.
    pub fn set_rollup_refresh_status(&self, status: &RefreshJobStatus) {
        let attrs = [KeyValue::new(label::REFRESH_POLICY, status.policy.as_label())];
        self.rollup_refresh_job_failing
            .record(u64::from(status.failing), &attrs);
        if let Some(age) = status.secs_since_success {
            self.rollup_refresh_age.record(age, &attrs);
        }
    }
```

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS.

- [ ] **Step 4: Run the monitor in the background task** (`src/gear.rs`).

(a) Import `use crate::infra::storage::rollup_maintenance::RollupMonitor;` and `use toolkit::tokio::time::MissedTickBehavior;`.

(b) Add `monitor: Arc<RollupMonitor>,` to `SweepWiring`. In `init`, build it before `metrics` is moved into `PgRecordStore::new`. Put this next to the sweeper construction:

```rust
        let monitor = Arc::new(RollupMonitor::new(pool.clone(), Arc::clone(&metrics)));
```

Set `monitor` in the `SweepWiring { … }` literal.

(c) In `start`, clone it (`let monitor = Arc::clone(&wiring.monitor);`) and spawn `run_background(sweeper, interval, monitor, token)` instead of `run_sweeps(…)`.

(d) Replace `run_sweeps` with:

```rust
/// How often refresh-policy health is sampled.
const ROLLUP_MONITOR_INTERVAL: Duration = Duration::from_secs(60);

/// Sweep and sample now, then each on its own interval, until `cancel` fires.
///
/// Cancellation is observed between operations only. A sweep holds the sweep
/// lock and may be part-way through a chunk drop; letting it finish is cheaper
/// than reasoning about where it stopped, and its lock connection closes either
/// way.
async fn run_background(
    sweeper: Arc<PgRetentionSweeper>,
    sweep_interval: Duration,
    monitor: Arc<RollupMonitor>,
    cancel: CancellationToken,
) {
    let mut sweep_tick = toolkit::tokio::time::interval(sweep_interval);
    sweep_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut monitor_tick = toolkit::tokio::time::interval(ROLLUP_MONITOR_INTERVAL);
    monitor_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        toolkit::tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            _ = sweep_tick.tick() => sweep_and_log(&sweeper).await,
            _ = monitor_tick.tick() => sample_and_log(&monitor).await,
        }
    }
}

/// Sample refresh-policy health once and log a failure.
async fn sample_and_log(monitor: &RollupMonitor) {
    match monitor.sample_once().await {
        Ok(n) => tracing::debug!(policies = n, "rollup refresh health sampled"),
        Err(e) => tracing::warn!(error = %e, "sampling rollup refresh health failed; retrying next interval"),
    }
}
```

`tokio::time::interval`'s first tick fires immediately, so the sweep still runs at start as before. Update the `start` log message to `"retention sweep and rollup monitor started"`.

- [ ] **Step 5: Write the integration test.** Append to `tests/rollup_aggregate_integration_pg.rs`, adding these imports:

```rust
use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;
use timescaledb_usage_collector_plugin::infra::storage::rollup_maintenance::refresh_job_statuses;
```

```rust
/// Re-applied policies run on creation, and the status query reads both back
/// with a success age once they have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_refresh_policies_report_a_success_age_after_their_first_run() {
    let s = stores().await;
    apply_post_migration_setup(&s.h.pool, &s.h.cfg).await.expect("re-apply policies");

    let mut statuses = Vec::new();
    for _ in 0..60 {
        statuses = refresh_job_statuses(&s.h.pool).await.expect("status query");
        if statuses.len() == 2 && statuses.iter().all(|st| st.secs_since_success.is_some()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let mut policies: Vec<_> = statuses.iter().map(|st| st.policy.as_label()).collect();
    policies.sort_unstable();
    assert_eq!(policies, ["history", "live"], "{statuses:?}");
    assert!(
        statuses.iter().all(|st| !st.failing && st.secs_since_success.is_some()),
        "{statuses:?}"
    );
}
```

- [ ] **Step 6: Run everything touched**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test rollup_aggregate_integration_pg`
Expected: PASS.

- [ ] **Step 7: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
P=gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git add $P/src/infra/storage/rollup_maintenance.rs $P/src/infra/storage/rollup_maintenance_tests.rs \
        $P/src/infra/metrics.rs $P/src/infra/metrics_tests.rs $P/src/gear.rs \
        $P/tests/rollup_aggregate_integration_pg.rs
git commit -m "feat(timescaledb-plugin): publish rollup refresh-policy health from the background task"
```

---

### Task 8: Document the aggregate path

**Files:**
- Modify: `README.md`
- Modify: `DIVERGENCES.md` (repository root, entry 21 only)

**Interfaces:**
- Consumes: the behaviour and names of Tasks 1–7.
- Produces: documentation only.

- [ ] **Step 1: Add an "Aggregate path" section** to `README.md`, directly before `## SPI conformance`:

```markdown
## Aggregate path

This section is the plugin's deployment-guide statement of acceptance → aggregate visibility and of how an accepted invalidation reaches the aggregate (the gear's `DESIGN.md` §3.10 item 3, `cpt-cf-usage-collector-nfr-aggregate-freshness`).

- **What is materialised.** `usage_rollup_1h`, an hourly TimescaleDB continuous aggregate over `usage_records` keyed `(bucket, tenant_id, gts_type_id, type_key)`. It holds a signed sum (`+value` for a record, `-value` for an invalidation) and a signed count.
- **When it serves a query.** All five must hold: the fold is `SUM` or `COUNT`; there is no metadata filter; `group_by` is empty or `tenant_id` alone; the composed filter (caller filter plus PDP scope) names only `tenant_id`; and the range covers at least one whole UTC hour. Whole hours are read from the rollup, the partial hours at either end from the ledger, in one statement. Every other query is the exact ledger scan. `uc_timescaledb_aggregate_path_total{path,reason}` shows the split.
- **Why the signed sum is exact.** An invalidation copies its target's `window_end`, `tenant_id` and type, so both land in one rollup row. A record carries at most one invalidation (`DIVERGENCES.md` entry 21). The target existed when the invalidation was accepted. The pair shares a chunk, so retention drops it together. A filter on `origin`, `entry_type` or `invalidates` can select one entry of a pair and not the other, which is why those queries take the scan.
- **Refresh.** Two policies, re-applied at startup: a live policy over the last `rollup_live_window_secs` every `rollup_refresh_interval_secs`, and a history policy over everything older every `rollup_history_refresh_interval_secs`. Buckets newer than `rollup_materialization_lag_secs` are never materialised; real-time aggregation reads them from the ledger. TimescaleDB background workers must be enabled.

| Entry's `window_end` | Acceptance → aggregate visibility | Invalidation propagation |
| --- | --- | --- |
| Within `rollup_materialization_lag_secs` of now | Immediate | Immediate |
| Older, within `rollup_live_window_secs` | ≤ `rollup_refresh_interval_secs` + refresh runtime | Same |
| Older than `rollup_live_window_secs` | ≤ `rollup_history_refresh_interval_secs` + refresh runtime | Same |
| Any query that takes the scan | Immediate | Immediate |

With the defaults: a late live write or a recent withdrawal appears within 2 minutes plus refresh runtime, and a backfilled period within 1 hour plus refresh runtime. These are configured bounds, not measurements. p95 conformance under `cpt-cf-usage-collector-nfr-throughput-profile` needs a load test, and this repository does not run one. Size `rollup_live_window_secs` to the gateway's live past tolerance, since a wider tolerance only moves those writes onto the history schedule.

- **Health.** `uc_timescaledb_rollup_refresh_age_seconds{policy}` is the time since each policy last succeeded, and `uc_timescaledb_rollup_refresh_job_failing{policy}` is 1 when its last run failed. Alert when the live age exceeds twice `rollup_refresh_interval_secs`, or when either failing gauge is 1.
```

- [ ] **Step 2: Extend the Retention section.** After the "**How.**" bullet in `## Retention`, add:

```markdown
- **The rollup goes with the ledger.** Dropping a chunk and deleting the rollup rows it fed happen in one transaction, so the aggregate never states anything the stored entries do not. If the rollup's table cannot be found, the sweep drops nothing. Deleted rows are counted by `uc_timescaledb_rollup_rows_deleted_total`.
```

Append `, uc_timescaledb_rollup_rows_deleted_total` to the "Sweep metrics:" line. In "Storage semantics", add this bullet after **Invalidation**:

```markdown
- **Aggregates** — eligible `SUM`/`COUNT` queries are served from the hourly rollup; see [Aggregate path](#aggregate-path).
```

- [ ] **Step 3: Extend `DIVERGENCES.md` entry 21.** Insert this paragraph immediately before the `---` line that precedes `## 22.`:

```markdown
**The rollup path depends on the same guarantee.** The TimescaleDB plugin's
hourly rollup (`usage_rollup_1h`, `migrations/0002_usage_rollup.sql`) nets a
withdrawn pair by adding `-value` for the invalidation rather than excluding
the pair. That is exact only while a record carries at most one invalidation: a
second one accepted past this gap would subtract the quantity twice. The scan
path miscounts such a pair too, so a violation now skews two reads rather than
one, not a new class of error.
```

- [ ] **Step 4: Verify the whole crate, then commit**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres`
Expected: PASS.

Run: `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings`
Expected: no warnings.

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/README.md DIVERGENCES.md
git commit -m "docs(timescaledb-plugin): publish the aggregate path's freshness and the rollup's retention coupling"
```
