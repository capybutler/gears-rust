# TimescaleDB Plugin — Per-Type Retention Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the plugin's single table-wide retention with per-GTS-type
retention. Each type's current declared `retention` is read from
`types-registry` and applied by dropping whole ledger chunks.

**Architecture:**
- The ledger hypertable gains a second partitioning dimension: a plugin-internal
  integer `type_key`, assigned once per type.
- Writes resolve the key from a cache.
- Reads prune to one type's chunks through an in-SQL subquery.
- A Rust background sweep does the dropping:
  1. It lists chunks from the TimescaleDB catalog.
  2. It resolves the retention of every type in each chunk's key range.
  3. It drops a chunk only when all of those types have expired.

**Tech Stack:** Rust 2024, `sqlx` (raw SQL), TimescaleDB 2.29.2 on PostgreSQL 18,
`cargo nextest`, `testcontainers`, `clippy::pedantic` (deny), OpenTelemetry
metrics, `types-registry-sdk`, `toolkit-utils`.

**Spec:** `docs/superpowers/specs/2026-09-14-usage-collector-timescaledb-per-type-retention-design.md`

## Global Constraints

**Repository and scope**
- Plugin root is `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`. Every relative path below is relative to it unless it starts with `gears/` or `docs/`.
- Package name: `cf-gears-timescaledb-usage-collector-plugin`.
- Do not edit `docs/DECOMPOSITION.md`, `docs/features/*`, the plugin's own `docs/`, `DIVERGENCES.md`, or the root memos (`TIMESCALEDB-RETENTION.md`, `AGGREGATION-UNDER-END-ASSIGNMENT.md`, `END-ASSIGNMENT-WITH-WINDOW-END-PARTITIONING.md`, `INVESTIGATION-PRE-AGGREGATION.md`).

**TimescaleDB**
- Target image: `timescale/timescaledb:2.29.2-pg18`, pinned by `test_containers::TIMESCALEDB_TAG`.
- Every unique constraint on the hypertable must contain every partitioning column: `window_end` and `type_key`.
- With two dimensions, `set_chunk_time_interval` requires an explicit `dimension_name`.

**Migration file**
- `migrations/0001_init.sql` is edited in place; there is no new migration file.
- `CREATE TABLE IF NOT EXISTS usage_records (` must stay verbatim.
- Every column line is indented exactly four spaces and spells its type as a single word. `src/infra/storage/migration_probe.rs` parses both.

**Rust conventions**
- No `unwrap`/`expect`/`panic!` outside tests, and no `as` numeric casts (use `try_from` / `from`).
- Test modules are sibling files wired as `#[cfg(test)] #[cfg_attr(coverage_nightly, coverage(off))] #[path = "<name>_tests.rs"] mod <name>_tests;`.
- Files issuing raw `sqlx` SQL start with `#![allow(unknown_lints, de0706_no_direct_sqlx)]`.

**Configuration**
- Config durations are whole seconds.
- `TimescaleDbPluginConfig` is `deny_unknown_fields`.
- Defaults: `chunk_time_interval_secs = 604_800`, `type_key_slice_width = 1`, `retention_sweep_interval_secs = 3_600`.

**Metrics**
- Instrument names are full literal Prometheus names under `uc_timescaledb_`.
- Counters end in `_total`; histograms built on `DURATION_BOUNDARIES_SECS` end in `_seconds`.
- Labels take bounded value sets only.

**Fail-safe:** the sweep never drops a chunk unless it holds a definite retention for every type in the chunk's key range.

**Commits:** conventional commits scoped `timescaledb-plugin`, with no attribution trailers.

**Commands**

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
| `migrations/0001_init.sql` | modify | `type_key` column, `usage_type_key` table, second dimension, widened constraints |
| `src/infra/storage/entity.rs` | modify | `UsageRecordRow.type_key` |
| `src/infra/storage/type_key.rs` (+ `_tests.rs`) | create | `TypeKeyCache`: resolve and remember a type's key |
| `src/infra/storage/record_store.rs` (+ `_tests.rs`) | modify | Bind `type_key` on both insert paths; column constants |
| `src/infra/storage/mapper_tests.rs` | modify | Row literals gain `type_key` |
| `src/infra/storage/query.rs` (+ `query_tests.rs`) | modify | Range builder adds the `type_key` subquery clause |
| `src/infra/storage/query/aggregate.rs` (+ `_tests.rs`) | modify | Withdrawal subquery correlates on `type_key`; alias guard admits `k` |
| `src/config.rs` (+ `config_tests.rs`) | modify | Remove `retention_period_secs`; add three keys |
| `src/infra/storage/pool.rs` | modify | Setup removes any policy and applies both intervals |
| `src/domain/ports.rs` | modify | `RetentionSource`, `RetentionError` |
| `src/domain/retention.rs` (+ `_tests.rs`), `src/domain.rs` | create / modify | `drop_decision`, `Decision`, `KeepReason` |
| `src/infra/registry_retention.rs` (+ `_tests.rs`), `src/infra.rs` | create / modify | `TypesRegistryRetentionSource`, `retention_from_traits` |
| `src/infra/metrics.rs` (+ `metrics_tests.rs`) | modify | Six retention instruments |
| `src/infra/storage/retention_sweep.rs` (+ `_tests.rs`), `src/infra/storage.rs` | create / modify | `PgRetentionSweeper`, `list_chunks`, `SweepReport` |
| `src/gear.rs` (+ `gear_tests.rs`) | modify | `stateful` capability; sweep loop lifecycle |
| `Cargo.toml` | modify | `toolkit-utils` dependency; `types-registry-sdk` `test-util` dev-dependency |
| `tests/common/mod.rs` | modify | Drop retention harness knobs |
| `tests/schema_integration_pg.rs` | modify | Dimensions, constraints, no policy, catalog pin |
| `tests/cleanup_integration_pg.rs` | modify | Keep the two lock tests; drop the policy tests |
| `tests/type_key_integration_pg.rs` | create | Key assignment under concurrency |
| `tests/records_query_integration_pg.rs` | modify | Single-type read executes one chunk |
| `tests/retention_sweep_integration_pg.rs` | create | Sweep behaviour over a stub retention source |
| `README.md` | modify | Config table, storage semantics, Retention section |

---

### Task 1: Partition the ledger on a per-type key (schema and write path)

**Files:**
- Modify: `migrations/0001_init.sql`
- Modify: `src/infra/storage/entity.rs`
- Create: `src/infra/storage/type_key.rs`, `src/infra/storage/type_key_tests.rs`
- Modify: `src/infra/storage.rs`
- Modify: `src/infra/storage/record_store.rs`
- Modify: `src/infra/storage/record_store_tests.rs`, `src/infra/storage/mapper_tests.rs`
- Modify: `tests/schema_integration_pg.rs`
- Create: `tests/type_key_integration_pg.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct TypeKeyCache` (`Debug`, `Default`), with `pub fn cached(&self, gts_type_id: &str) -> Option<i32>` and `pub async fn resolve(&self, conn: &mut sqlx::PgConnection, gts_type_id: &str) -> Result<i32, sqlx::Error>`.
  - Column `usage_records.type_key int NOT NULL`, and table `usage_type_key(gts_type_id text PK, type_key int identity UNIQUE)`.
  - `UsageRecordRow.type_key: i32`.

- [ ] **Step 1: Write the failing schema expectations**

In `tests/schema_integration_pg.rs`:

(a) In `canonical_type`, add an arm before `other => other`:

```rust
        "int" => "integer",
```

(b) Replace the body assertion of `the_ledger_is_a_hypertable_partitioned_on_window_end`. Rename the test to `the_ledger_partitions_on_window_end_then_type_key`:

```rust
    assert_eq!(
        dims,
        vec![
            ("window_end".to_owned(), 1_i64),
            ("type_key".to_owned(), 2_i64),
        ],
        "usage_records must partition on window_end first and on the per-type key second: \
         the key is what lets a chunk be dropped by the retention of the types in it"
    );
```

(c) In `the_dedup_unique_spans_the_five_tuple`, change the expected definition. Keep the existing message, and append the sentence below it:

```rust
    assert_eq!(
        def,
        "UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end, type_key)",
        "the dedup UNIQUE must span the 5-tuple dedup identity, in that order - the same \
         five inputs the entry id is a UUIDv5 projection of \
         (cpt-cf-usage-collector-adr-record-identity-derivation) - plus the partition key, \
         which a type determines and so separates no two rows the 5-tuple joins"
    );
```

(d) In `the_at_most_one_invalidation_index_is_partial_and_unique`, replace the `btree (invalidates, window_end)` assertion with:

```rust
    assert!(
        def.contains("btree (invalidates, window_end, type_key)"),
        "the index must lead with `invalidates` and carry both partition columns: {def}"
    );
```

(e) Append a new test:

```rust
/// The per-type key table: one row per type, keyed by the type, with a
/// database-assigned integer that nothing else may write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_type_key_maps_each_type_to_a_generated_integer() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT attname::text, format_type(atttypid, atttypmod), attidentity::text \
         FROM pg_attribute \
         WHERE attrelid = 'usage_type_key'::regclass AND attnum > 0 AND NOT attisdropped \
         ORDER BY attnum",
    )
    .fetch_all(&h.pool)
    .await
    .expect("usage_type_key must exist");

    assert_eq!(
        columns,
        vec![
            ("gts_type_id".to_owned(), "text".to_owned(), String::new()),
            ("type_key".to_owned(), "integer".to_owned(), "a".to_owned()),
        ],
        "usage_type_key is (gts_type_id text, type_key int GENERATED ALWAYS AS IDENTITY)"
    );
}
```

- [ ] **Step 2: Write the failing type-key integration test**

Create `tests/type_key_integration_pg.rs`:

```rust
#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The per-type partitioning key, assigned on first write. Requires Docker.

mod common;

use rust_decimal::Decimal;
use uuid::Uuid;

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;

/// Eight stores, each with a cold key cache, write the first entry of one type
/// at once. Every one takes the database path, so a race in the assignment
/// would show up as two keys for one type.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_writes_of_one_type_share_one_key() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let meter = common::meter(common::VCPU_METER);

    let mut tasks = Vec::new();
    for i in 0..8u128 {
        let store = common::record_store(&h.pool);
        let meter = meter.clone();
        tasks.push(tokio::spawn(async move {
            store
                .create(common::entry(
                    &meter,
                    Uuid::from_u128(0x7E00 + i),
                    "first",
                    Decimal::ONE,
                ))
                .await
        }));
    }
    for task in tasks {
        task.await
            .expect("task did not panic")
            .expect("a concurrent first write must succeed");
    }

    let keys: Vec<i32> = sqlx::query_scalar("SELECT DISTINCT type_key FROM usage_records")
        .fetch_all(&h.pool)
        .await
        .expect("read ledger keys");
    assert_eq!(keys.len(), 1, "one type must carry one key: {keys:?}");

    let mapped: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_type_key")
        .fetch_one(&h.pool)
        .await
        .expect("count mapped types");
    assert_eq!(mapped, 1, "one type must be mapped exactly once");
}

/// Two types written through each insert path get two keys, and every ledger
/// row carries the key its own type maps to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_row_carries_its_own_types_key_on_both_insert_paths() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x7E10);
    let vcpu = common::meter(common::VCPU_METER);
    let gb = common::meter(common::GB_METER);

    store
        .create(common::entry(&vcpu, tenant, "single-vcpu", Decimal::ONE))
        .await
        .expect("single insert");
    let outcomes = store
        .create_batch(vec![
            common::entry(&vcpu, tenant, "batch-vcpu", Decimal::ONE),
            common::entry(&gb, tenant, "batch-gb", Decimal::ONE),
        ])
        .await
        .expect("batch insert");
    assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");

    let rows: Vec<(String, i32, i32)> = sqlx::query_as(
        "SELECT r.gts_type_id, r.type_key, k.type_key \
         FROM usage_records r JOIN usage_type_key k USING (gts_type_id) \
         ORDER BY r.idempotency_key",
    )
    .fetch_all(&h.pool)
    .await
    .expect("read rows with their mapped keys");

    assert_eq!(rows.len(), 3);
    for (gts_type_id, row_key, mapped_key) in &rows {
        assert_eq!(row_key, mapped_key, "{gts_type_id} must carry its mapped key");
    }
    let vcpu_key = rows.iter().find(|r| r.0 == common::VCPU_METER).unwrap().1;
    let gb_key = rows.iter().find(|r| r.0 == common::GB_METER).unwrap().1;
    assert_ne!(vcpu_key, gb_key, "distinct types must get distinct keys");
}
```

- [ ] **Step 3: Run the new integration tests to verify they fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test schema_integration_pg --test type_key_integration_pg`

Expected: FAIL. The dimensions assertion sees only `window_end`, `usage_type_key` does not exist, and the type-key queries error with `column r.type_key does not exist`.

- [ ] **Step 4: Change the migration**

In `migrations/0001_init.sql`:

(a) Directly after the line `    gts_type_id         text        NOT NULL,`, insert:

```sql
    -- The plugin-internal integer key of `gts_type_id`, assigned once per type
    -- from `usage_type_key` below. It is the hypertable's second partitioning
    -- dimension, so a chunk holds a slice of types and the retention sweep can
    -- drop it by the retention of the types in it. It is not a declared
    -- attribute (cpt-cf-usage-collector-adr-declaration-rehydration statement
    -- 6): it names the type, and a type's key never changes.
    type_key            int         NOT NULL,
```

(b) Replace the primary-key comment paragraph's first sentence, and the key itself, with:

```sql
    -- A hypertable's PRIMARY KEY and every UNIQUE must contain every partition
    -- column, so both carry `window_end` and `type_key`. `type_key` is a
    -- function of `gts_type_id`, so adding it separates no two rows either key
    -- would otherwise join.
```

Keep the existing paragraph that follows ("This is the same key as the dedup UNIQUE below…"), then:

```sql
    PRIMARY KEY (id, window_end, type_key),
```

(c) Replace the dedup constraint with:

```sql
    -- The gear's DESIGN §3.7 dedup obligation, over the 5-tuple verbatim, plus
    -- the partition key the hypertable requires (see the PRIMARY KEY above).
    CONSTRAINT usage_records_dedup_uniq
        UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end, type_key),
```

(d) Replace `SELECT create_hypertable('usage_records', 'window_end', if_not_exists => TRUE);` with:

```sql
-- Partitioned on the covered-period end, then on the per-type key. The key's
-- slice width and the time interval are configuration, applied at startup to
-- chunks created afterwards (`pool::apply_post_migration_setup`).
SELECT create_hypertable('usage_records', by_range('window_end'), if_not_exists => TRUE);
SELECT add_dimension('usage_records', by_range('type_key', 1), if_not_exists => TRUE);
```

(e) In the at-most-one-invalidation index comment, change "including the partition column costs nothing" to "including the partition columns costs nothing — an invalidation also shares its target's type, and so its `type_key`". Then change the index to:

```sql
CREATE UNIQUE INDEX IF NOT EXISTS usage_records_one_invalidation_uniq
    ON usage_records (invalidates, window_end, type_key)
    WHERE invalidates IS NOT NULL;
```

(f) After the `usage_acceptance_sequence` table, add:

```sql
-- Per-type partitioning keys.
--
-- One row per GTS type this plugin has written, mapping the type to a small
-- integer the hypertable can partition on (`by_range` refuses a text column).
-- It stores no declared attribute and nothing references it; it is not a type
-- catalog. A key is assigned by the first write of its type and never changes,
-- which is what lets it sit inside the ledger's unique constraints.
CREATE TABLE IF NOT EXISTS usage_type_key (
    gts_type_id text NOT NULL PRIMARY KEY,
    type_key    int  GENERATED ALWAYS AS IDENTITY UNIQUE
);
```

- [ ] **Step 5: Run the unit tests to see the column pins fail against the new migration**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`

Expected: FAIL in `record_store_tests`. The inserted column sequence no longer matches `migration_probe::insertable_columns()`: the migration has `type_key` after `gts_type_id` and `INSERT_COLUMNS` does not.

- [ ] **Step 6: Create the key cache**

Create `src/infra/storage/type_key.rs`:

```rust
//! The plugin-internal integer key of each GTS type (`usage_type_key`).
//!
//! The key is the ledger hypertable's second partitioning dimension. It names a
//! type and never changes, so a resolved key is kept for the life of the
//! process and no entry ever expires. Only the write path uses this cache: the
//! read paths resolve the key inside their own statement
//! ([`super::query::push_meter_and_range_clauses`]).

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (see
// `record_store.rs`).
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};

use sqlx::PgConnection;

/// Assigns a key to a type that has none. A concurrent assignment of the same
/// type waits on the speculative row and then does nothing, so the read that
/// follows sees exactly one key.
pub const ASSIGN_TYPE_KEY_SQL: &str =
    "INSERT INTO usage_type_key (gts_type_id) VALUES ($1) ON CONFLICT (gts_type_id) DO NOTHING";

/// Reads a type's key.
pub const READ_TYPE_KEY_SQL: &str = "SELECT type_key FROM usage_type_key WHERE gts_type_id = $1";

/// Process-wide cache of resolved type keys.
#[derive(Debug, Default)]
pub struct TypeKeyCache {
    keys: RwLock<HashMap<String, i32>>,
}

impl TypeKeyCache {
    /// The key already resolved for `gts_type_id`, if any.
    #[must_use]
    pub fn cached(&self, gts_type_id: &str) -> Option<i32> {
        self.keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(gts_type_id)
            .copied()
    }

    /// The key of `gts_type_id`, assigning one on its first write.
    ///
    /// Runs in autocommit on `conn`, so call it **before** opening the write
    /// transaction: a key assigned inside a transaction that then rolls back
    /// would make every concurrent first writer of the type wait on it for
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns the `sqlx` error of either statement.
    pub async fn resolve(
        &self,
        conn: &mut PgConnection,
        gts_type_id: &str,
    ) -> Result<i32, sqlx::Error> {
        if let Some(key) = self.cached(gts_type_id) {
            return Ok(key);
        }
        sqlx::query(ASSIGN_TYPE_KEY_SQL)
            .bind(gts_type_id)
            .execute(&mut *conn)
            .await?;
        let key: i32 = sqlx::query_scalar(READ_TYPE_KEY_SQL)
            .bind(gts_type_id)
            .fetch_one(&mut *conn)
            .await?;
        self.remember(gts_type_id, key);
        Ok(key)
    }

    fn remember(&self, gts_type_id: &str, key: i32) {
        self.keys
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(gts_type_id.to_owned(), key);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "type_key_tests.rs"]
mod type_key_tests;
```

Create `src/infra/storage/type_key_tests.rs`:

```rust
use super::TypeKeyCache;

#[test]
fn an_unresolved_type_has_no_cached_key() {
    let cache = TypeKeyCache::default();
    assert_eq!(cache.cached("gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~"), None);
}

#[test]
fn a_remembered_key_is_served_and_is_per_type() {
    let cache = TypeKeyCache::default();
    cache.remember("type-a~", 7);
    assert_eq!(cache.cached("type-a~"), Some(7));
    assert_eq!(cache.cached("type-b~"), None, "a key belongs to its own type only");
}
```

In `src/infra/storage.rs`, add `pub mod type_key;` after `pub mod record_store;`.

- [ ] **Step 7: Add the row field**

In `src/infra/storage/entity.rs`:
- Add `pub type_key: i32,` directly after `pub gts_type_id: String,`.
- In the module doc's type list, add `` `int` → `i32` ``.

In every `UsageRecordRow { … }` literal, add `type_key: 1,` directly after the `gts_type_id` field:
- `src/infra/storage/mapper_tests.rs` (lines 182–372)
- `src/infra/storage/record_store_tests.rs` (`row_matching`, `keyed_row`, `list_row`)

Find them all with `grep -n "UsageRecordRow {" src`.

- [ ] **Step 8: Bind the key on both insert paths**

In `src/infra/storage/record_store.rs`:

(a) Add `use crate::infra::storage::type_key::TypeKeyCache;` to the imports.

(b) Constants:

```rust
const RECORD_COLUMNS: &str = "id, tenant_id, gts_type_id, type_key, value, window_start, \
     window_end, resource_id, resource_type, subject_id, subject_type, idempotency_key, \
     invalidates, reason_code, origin, acceptance_sequence, metadata, ingested_at";
```

```rust
const INSERT_COLUMNS: &str = "id, tenant_id, gts_type_id, type_key, value, window_start, \
     window_end, resource_id, resource_type, subject_id, subject_type, idempotency_key, \
     invalidates, reason_code, origin, acceptance_sequence, metadata";
```

`INSERT_COLUMN_ARRAY_TYPES` becomes `[&str; 17]`, with `"int",` inserted after the first `"text",`:

```rust
const INSERT_COLUMN_ARRAY_TYPES: [&str; 17] = [
    "uuid",
    "uuid",
    "text",
    "int",
    "numeric",
    "timestamptz",
    "timestamptz",
    "text",
    "text",
    "text",
    "text",
    "text",
    "uuid",
    "text",
    "text",
    "bigint",
    "text",
];
```

```rust
const DEDUP_CONFLICT_TARGET: &str =
    "tenant_id, gts_type_id, idempotency_key, window_start, window_end, type_key";
```

Update the doc sentence on `DEDUP_CONFLICT_TARGET` to "The dedup 5-tuple plus the partition key the hypertable requires in every UNIQUE, as an `ON CONFLICT` arbiter."

(c) Struct and constructor:

```rust
#[derive(Debug, Clone)]
pub struct PgRecordStore {
    pool: PgPool,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
    type_keys: Arc<TypeKeyCache>,
}
```

In `new`, add `type_keys: Arc::new(TypeKeyCache::default()),`.

(d) In `create_inner`, directly after `let mut conn = self.timed_acquire().await?;` and before `conn.begin()`:

```rust
        // The type's partition key, resolved in autocommit before the write
        // transaction opens (see `TypeKeyCache::resolve`).
        let type_key = self
            .type_keys
            .resolve(&mut conn, record.gts_type_id.as_str())
            .await
            .map_err(|e| self.record_backend_error(&e))?;
```

Then add `.bind(type_key)` directly after `.bind(record.gts_type_id.as_str())` in the single-insert bind chain. Change the comment "which is why sixteen of the seventeen [`RECORD_COLUMNS`] are bound here" to "which is why seventeen of the eighteen [`RECORD_COLUMNS`] are bound here".

(e) In `create_batch_inner`, directly after `let mut conn = self.timed_acquire().await?;` and before `conn.begin()`:

```rust
        // One partition key per representative, aligned to `plan.reps`, resolved
        // in autocommit before the write transaction opens. A type already seen
        // costs no round trip.
        let mut type_keys: Vec<i32> = Vec::with_capacity(plan.reps.len());
        for rep in &plan.reps {
            let key = self
                .type_keys
                .resolve(&mut conn, rep.gts_type_id.as_str())
                .await
                .map_err(|e| self.record_backend_error(&e))?;
            type_keys.push(key);
        }
```

Change the insert call to `Self::insert_records_on_conflict(&mut tx, &plan.reps, &sequences, &type_keys).await`.

(f) `insert_records_on_conflict` gains a `type_keys: &[i32]` parameter after `sequences`. It calls `InsertColumns::build(reps, sequences, type_keys)` and adds `.bind(&cols.type_keys)` directly after `.bind(&cols.gts_type_ids)`. Add to its doc: "`type_keys` must be the partition keys resolved for `reps`, in the same order."

(g) `InsertColumns`:
- Doc "The sixteen per-column vectors" becomes "The seventeen per-column vectors".
- Add field `type_keys: Vec<i32>,` after `gts_type_ids`.
- `build` becomes `fn build(reps: &[&UsageRecord], sequences: &[i64], type_keys: &[i32]) -> Self`. Add a second assertion after the sequences one:

```rust
        assert_eq!(
            reps.len(),
            type_keys.len(),
            "one partition key must be resolved per batch representative"
        );
```

- Initialise `type_keys: type_keys.to_vec(),` in the struct literal, and extend the `# Panics` doc to name the keys too.

(h) In `src/infra/storage/record_store_tests.rs`:
- Both `"ON CONFLICT (tenant_id, gts_type_id, idempotency_key, window_start, window_end)"` literals (around lines 494 and 557) become `"ON CONFLICT (tenant_id, gts_type_id, idempotency_key, window_start, window_end, type_key)"`.
- Around line 724, `InsertColumns::build(&[&plain, &with], &[7, 8])` becomes `InsertColumns::build(&[&plain, &with], &[7, 8], &[3, 4])`. Add next to the existing `cols.ids` assertion:

```rust
    assert_eq!(cols.type_keys, vec![3, 4], "each representative's partition key, in order");
```

- [ ] **Step 9: Run the unit tests to verify they pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`

Expected: PASS.

- [ ] **Step 10: Run the Docker suites to verify they pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres`

Expected: PASS, including `schema_integration_pg`, `type_key_integration_pg`, `records_ingest_integration_pg`, `records_query_integration_pg`, `id_uniqueness_integration_pg` and `contract_conformance_pg`.

`cleanup_integration_pg` still passes here because the table-wide policy is still registered. Task 3 retires it.

- [ ] **Step 11: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "feat(timescaledb-plugin): partition the ledger on a per-type key"
```

---

### Task 2: Prune single-type reads to that type's chunks

**Files:**
- Modify: `src/infra/storage/query.rs`, `src/infra/storage/query_tests.rs`
- Modify: `src/infra/storage/query/aggregate.rs`, `src/infra/storage/query/aggregate_tests.rs`
- Modify: `src/infra/storage/record_store.rs` (doc comment only), `src/infra/storage/record_store_tests.rs`
- Modify: `tests/records_query_integration_pg.rs`

**Interfaces:**
- Consumes: `usage_type_key` and `usage_records.type_key` from Task 1.
- Produces:
  - `push_meter_and_range_clauses` pushes four clauses: meter, key subquery, lower bound, upper bound. It still pushes three binds.
  - `withdrawal_exclusion_clause()` returns the `type_key`-correlated form.

- [ ] **Step 1: Write the failing pins**

In `src/infra/storage/query_tests.rs`, replace the expected clause vector with:

```rust
    assert_eq!(
        clauses,
        vec![
            "r.gts_type_id = $1".to_owned(),
            "r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = $1)"
                .to_owned(),
            "r.window_end >= $2".to_owned(),
            "r.window_end < $3".to_owned(),
        ]
    );
```

Add below it, in the same test:

```rust
    assert_eq!(
        ctx.binds.len(),
        3,
        "the key subquery reuses the meter's bind rather than binding it twice"
    );
```

In `src/infra/storage/query/aggregate_tests.rs`:
- Replace the `NOT EXISTS` substring in `a_record_an_accepted_invalidation_names_is_excluded_under_every_fold` with `"NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id AND w.type_key = r.type_key)"`.
- Replace the whole-clause literal in `no_fold_gets_its_own_withdrawal_rule` with:

```rust
    assert_eq!(
        withdrawal_exclusion_clause(),
        "r.invalidates IS NULL \
         AND NOT EXISTS (SELECT 1 FROM usage_records w \
         WHERE w.invalidates = r.id AND w.type_key = r.type_key)"
    );
```

In the same file:
- Add `"type_key",` to `LEDGER_COLUMNS` directly after `"gts_type_id",`.
- Replace `aliases_for` with:

```rust
/// The aliases `sql` may qualify a column with. `r`, the alias
/// [`ledger_from_clause`] declares, is always admissible. `w` and `k` are
/// admissible only in a fragment that opens them — the withdrawal subquery's
/// second ledger and the type-key lookup's `usage_type_key` — so a fragment
/// borrowing either without opening it is an offender rather than a pass.
fn aliases_for(sql: &str) -> &'static [u8] {
    match (
        sql.contains("FROM usage_records w"),
        sql.contains("FROM usage_type_key k"),
    ) {
        (true, true) => b"rwk",
        (true, false) => b"rw",
        (false, true) => b"rk",
        (false, false) => b"r",
    }
}
```

In `src/infra/storage/record_store_tests.rs`, in each of the four statement pins (around lines 1884, 1954, 1983 and 2279), replace

`r.gts_type_id = $1 AND r.window_end >= $2`

with

`r.gts_type_id = $1 AND r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = $1) AND r.window_end >= $2`

Split the Rust string across `\` continuations to match the surrounding style. Around line 2281, replace `AND NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id) \` with `AND NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id AND w.type_key = r.type_key) \`. Bind counts in those tests do not change.

- [ ] **Step 2: Run the unit tests to verify they fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`

Expected: FAIL on the four statement pins, the query clause vector, and the two withdrawal pins. `ledger_columns_are_the_migrations_columns` now passes, because `LEDGER_COLUMNS` has `type_key`.

- [ ] **Step 3: Implement the clauses**

In `src/infra/storage/query.rs`, replace the body of `push_meter_and_range_clauses` with:

```rust
    let meter = ctx.push(SqlBind::Str(gts_type_id.as_str().to_owned()));
    clauses.push(format!("r.gts_type_id = ${meter}"));
    // The same meter as its partition key, which is what excludes every other
    // type's chunks: a predicate on `gts_type_id` alone excludes none. The
    // subquery runs once as an InitPlan and the chunks it rules out are skipped
    // at runtime. A type that was never written has no key, so it yields NULL,
    // matches no row and excludes every chunk — the ordinary empty result, with
    // no sentinel and no lookup ahead of the statement.
    clauses.push(format!(
        "r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = ${meter})"
    ));
    clauses.push(format!(
        "r.window_end >= ${}",
        ctx.push(SqlBind::DateTime(time_range.lower_inclusive()))
    ));
    clauses.push(format!(
        "r.window_end < ${}",
        ctx.push(SqlBind::DateTime(time_range.upper_exclusive()))
    ));
```

In its doc, change "and pushes the three clause strings onto `clauses`" to "and pushes four clause strings onto `clauses`. The partition-key subquery reuses the meter's bind."

In `src/infra/storage/query/aggregate.rs`, `withdrawal_exclusion_clause` returns:

```rust
    "r.invalidates IS NULL \
     AND NOT EXISTS (SELECT 1 FROM usage_records w \
     WHERE w.invalidates = r.id AND w.type_key = r.type_key)"
```

Add a paragraph to its doc:

```rust
/// The subquery also pins `w.type_key = r.type_key`. An invalidation has its
/// target's type and so its key, so this excludes nothing the `id` match would
/// keep; what it buys is chunk exclusion inside the subquery, which otherwise
/// probes the invalidation index in every type's chunks.
```

In `src/infra/storage/record_store.rs`, update the `build_aggregate_sql` doc's SQL sketch line to `/// WHERE r.gts_type_id = $1 AND r.type_key = (…) AND r.window_end >= $2 AND r.window_end < $3`.

- [ ] **Step 4: Run the unit tests to verify they pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`

Expected: PASS.

- [ ] **Step 5: Write the pruning integration test**

Append to `tests/records_query_integration_pg.rs`. Add any of these `use` lines not already present at the top: `sqlx::AssertSqlSafe`, `time::Duration`, `usage_collector_sdk::TimeRange`, `timescaledb_usage_collector_plugin::infra::storage::query::{ledger_from_clause, push_meter_and_range_clauses}`, `timescaledb_usage_collector_plugin::infra::storage::query::translate::SqlCtx`.

```rust
/// Two types in one time range sit in two chunks. A read of one type must
/// execute only its own chunk. `gts_type_id` alone excludes nothing, so this
/// fails if the partition-key clause is missing or stops excluding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_type_read_executes_only_that_types_chunk() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x9A0E);
    let vcpu = common::meter(common::VCPU_METER);
    let gb = common::meter(common::GB_METER);
    store
        .create(common::entry(&vcpu, tenant, "vcpu", Decimal::ONE))
        .await
        .expect("create vcpu");
    store
        .create(common::entry(&gb, tenant, "gb", Decimal::ONE))
        .await
        .expect("create gb");

    let range = TimeRange::new(
        common::fixture_window_start(),
        common::fixture_window_end() + Duration::seconds(1),
    )
    .expect("a strictly ordered range");
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();
    push_meter_and_range_clauses(&vcpu, range, &mut ctx, &mut clauses);
    let sql = format!(
        "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT count(*) FROM {} WHERE {}",
        ledger_from_clause(),
        clauses.join(" AND "),
    );

    let plan: Vec<String> = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(vcpu.as_str())
        .bind(range.lower_inclusive())
        .bind(range.upper_exclusive())
        .fetch_all(&h.pool)
        .await
        .expect("explain the single-type read");

    let executed = plan
        .iter()
        .filter(|line| line.contains("_hyper_") && !line.contains("never executed"))
        .count();
    assert_eq!(
        executed, 1,
        "only the queried type's chunk may execute:\n{}",
        plan.join("\n")
    );
}
```

- [ ] **Step 6: Run the query suite**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test records_query_integration_pg --test contract_conformance_pg`

Expected: PASS.

To confirm the test discriminates, temporarily delete the `r.type_key = …` push in `query.rs` and re-run `a_single_type_read_executes_only_that_types_chunk`: it must FAIL with `executed == 2`. Restore the line before continuing.

- [ ] **Step 7: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "perf(timescaledb-plugin): exclude other types' chunks from single-type reads"
```

---

### Task 3: Replace the table-wide retention policy with partitioning configuration

**Files:**
- Modify: `src/config.rs`, `src/config_tests.rs`
- Modify: `src/infra/storage/pool.rs`
- Modify: `src/gear.rs` (one call site)
- Modify: `tests/common/mod.rs`, `tests/cleanup_integration_pg.rs`, `tests/schema_integration_pg.rs`
- Modify: `README.md` (configuration table and example only)

**Interfaces:**
- Consumes: the `type_key` dimension from Task 1.
- Produces:
  - `TimescaleDbPluginConfig` fields `chunk_time_interval_secs: u64`, `type_key_slice_width: u32` and `retention_sweep_interval_secs: u64`.
  - `pub async fn apply_post_migration_setup(pool: &PgPool, chunk_time_interval_secs: u64, type_key_slice_width: u32) -> Result<(), sqlx::Error>`.
  - `pub async fn apply_partitioning(pool: &PgPool, chunk_time_interval_secs: u64, type_key_slice_width: u32) -> Result<(), sqlx::Error>`.
  - `tests/common::bring_up_with(statement_timeout_secs: u64, pool_size_min: u32, pool_size_max: u32)`.

- [ ] **Step 1: Write the failing config tests**

In `src/config_tests.rs`:
- In `config_defaults_are_applied`, replace `assert_eq!(cfg.retention_period_secs, 365 * 86_400);` with:

```rust
    assert_eq!(cfg.chunk_time_interval_secs, 604_800);
    assert_eq!(cfg.type_key_slice_width, 1);
    assert_eq!(cfg.retention_sweep_interval_secs, 3_600);
```

- Delete `validate_rejects_zero_retention`, `validate_rejects_excessive_retention` and `validate_accepts_large_but_sane_retention`. Add:

```rust
#[test]
fn validate_rejects_zero_chunk_time_interval() {
    let json = r#"{ "database_url": "postgres://x", "chunk_time_interval_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err(), "a zero-width chunk cannot hold a row");
}

#[test]
fn validate_rejects_a_chunk_time_interval_beyond_the_interval_bound() {
    let json = format!(
        r#"{{ "database_url": "postgres://x", "chunk_time_interval_secs": {} }}"#,
        u64::MAX
    );
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(&json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "an interval make_interval cannot hold must fail before it reaches the database"
    );
}

#[test]
fn validate_rejects_zero_type_key_slice_width() {
    let json = r#"{ "database_url": "postgres://x", "type_key_slice_width": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err(), "a slice must hold at least one type key");
}

#[test]
fn validate_rejects_a_type_key_slice_width_wider_than_the_key_type() {
    let json = r#"{ "database_url": "postgres://x", "type_key_slice_width": 2147483648 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err(), "type_key is an int; a wider slice is meaningless");
}

#[test]
fn validate_rejects_zero_retention_sweep_interval() {
    let json = r#"{ "database_url": "postgres://x", "retention_sweep_interval_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err(), "a zero interval would sweep in a hot loop");
}

#[test]
fn config_rejects_the_retired_table_wide_retention_key() {
    // Retention is per type now, read from types-registry. A config still
    // carrying the table-wide window must fail loudly rather than be ignored.
    let json = r#"{ "database_url": "postgres://x", "retention_period_secs": 31536000 }"#;
    assert!(serde_json::from_str::<TimescaleDbPluginConfig>(json).is_err());
}
```

Update the comment in `validate_rejects_pool_max_of_one`: "while the retention policy tries to acquire a second" becomes "while the partitioning statements run on a second".

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib config_tests`

Expected: FAIL to compile, because `no field chunk_time_interval_secs`.

- [ ] **Step 3: Implement the config**

In `src/config.rs`, replace the `retention_period_secs` field and its doc with:

```rust
    /// Time width of a new ledger chunk, in seconds. Applied at startup to
    /// chunks created afterwards; existing chunks keep their range.
    pub chunk_time_interval_secs: u64,
    /// How many consecutive type keys share one slice of the ledger's second
    /// partitioning dimension. `1` gives every type its own chunks, so each is
    /// dropped exactly at its own retention; `N` lets up to `N` types share a
    /// chunk, which is then held to the longest retention among them. Applied at
    /// startup to chunks created afterwards.
    pub type_key_slice_width: u32,
    /// Seconds between two retention sweeps.
    pub retention_sweep_interval_secs: u64,
```

In `Default`, replace `retention_period_secs: 365 * 86_400, // 365 days` with:

```rust
            chunk_time_interval_secs: 7 * 86_400,
            type_key_slice_width: 1,
            retention_sweep_interval_secs: 3_600,
```

Replace `MAX_RETENTION_SECS` and its doc with:

```rust
/// Upper bound on every interval setting (100 years in seconds).
///
/// Postgres `make_interval(secs => ...)`, which applies the chunk interval,
/// overflows well below `u64::MAX`. A pathological value would otherwise surface
/// as a confusing failure *after* migrations have already run.
const MAX_INTERVAL_SECS: u64 = 100 * 365 * 86_400;

/// Upper bound on `type_key_slice_width`: `type_key` is an `int`.
const MAX_TYPE_KEY_SLICE_WIDTH: u32 = 2_147_483_647;
```

In `validate`:
- Update the pool-bounds comment and message: "while `apply_retention_policy` acquires a *second*" becomes "while `apply_partitioning` runs on a *second*", and "while the retention policy acquires a second" becomes "while the partitioning statements run on a second".
- Replace the two retention checks with:

```rust
        if self.chunk_time_interval_secs == 0 || self.chunk_time_interval_secs > MAX_INTERVAL_SECS {
            return Err(format!(
                "chunk_time_interval_secs must be in (0, {MAX_INTERVAL_SECS}] (100 years)"
            ));
        }
        if self.type_key_slice_width == 0 || self.type_key_slice_width > MAX_TYPE_KEY_SLICE_WIDTH {
            return Err(format!(
                "type_key_slice_width must be in [1, {MAX_TYPE_KEY_SLICE_WIDTH}]"
            ));
        }
        if self.retention_sweep_interval_secs == 0
            || self.retention_sweep_interval_secs > MAX_INTERVAL_SECS
        {
            return Err(format!(
                "retention_sweep_interval_secs must be in (0, {MAX_INTERVAL_SECS}] (100 years)"
            ));
        }
```

- Update the `# Errors` doc: "or a retention window outside `(0, MAX_RETENTION_SECS]`" becomes "an interval outside `(0, MAX_INTERVAL_SECS]`, or a slice width outside `[1, MAX_TYPE_KEY_SLICE_WIDTH]`".

- [ ] **Step 4: Run the config tests to verify they pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib config_tests`

Expected: the tests PASS. The crate as a whole does not compile yet, because `gear.rs` still passes `cfg.retention_period_secs`. The next step fixes that.

- [ ] **Step 5: Replace the policy registration**

In `src/infra/storage/pool.rs`, replace `apply_post_migration_setup` and `apply_retention_policy` with the code below. Keep `acquire_init_lock` and `INIT_ADVISORY_LOCK_KEY` unchanged.

```rust
/// Run the post-migration partitioning setup under a database advisory lock so
/// concurrently-initializing replicas serialize here.
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
/// `statement_timeout` (see [`acquire_init_lock`]) so a wedged peer cannot stall
/// init forever.
///
/// # Errors
/// Returns `sqlx::Error` if the lock cannot be acquired or a setup statement
/// fails.
pub async fn apply_post_migration_setup(
    pool: &PgPool,
    chunk_time_interval_secs: u64,
    type_key_slice_width: u32,
) -> Result<(), sqlx::Error> {
    let mut lock_conn = pool.acquire().await?;
    acquire_init_lock(&mut lock_conn).await?;

    let result = apply_partitioning(pool, chunk_time_interval_secs, type_key_slice_width).await;

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
```

In `src/gear.rs`:
- Replace `apply_post_migration_setup(&pool, cfg.retention_period_secs).await?;` with `apply_post_migration_setup(&pool, cfg.chunk_time_interval_secs, cfg.type_key_slice_width).await?;`.
- Change the comment "Connect, migrate, and install the config-driven retention policy." to "Connect, migrate, and apply the configured partitioning."

- [ ] **Step 6: Update the test harness**

In `tests/common/mod.rs`:
- Delete `NO_DROP_RETENTION_SECS`, `REAL_RETENTION_SECS` and `bring_up_real_retention`, with their doc comments.
- Change `bring_up` to `bring_up_with(30, 2, 16).await`, with its comment "Default pool bounds and statement timeout (mirrors the config defaults)."
- Change `bring_up_with` to take `(statement_timeout_secs: u64, pool_size_min: u32, pool_size_max: u32)`. Delete the `retention_secs` doc paragraph, and delete the `"retention_period_secs": {retention_secs}` line from the config JSON, keeping the JSON valid.
- Replace the setup call with `apply_post_migration_setup(&pool, cfg.chunk_time_interval_secs, cfg.type_key_slice_width).await?;`.
- Remove every remaining sentence that mentions `NO_DROP_RETENTION_SECS`, `policy_retention` or the scheduled retention job (`grep -n "RETENTION\|policy_retention" tests/common/mod.rs` must print nothing). Where such a sentence explained why fixtures are safe from drops, replace it with: "Nothing in the harness drops data: chunks are dropped only by the retention sweep, which a test runs explicitly."

- [ ] **Step 7: Rewrite the setup integration tests**

Replace `tests/cleanup_integration_pg.rs` entirely with:

```rust
#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `TimescaleDB`-backed tests for the post-migration partitioning setup:
//! concurrent-replica serialization and the pooled-connection hygiene of its
//! advisory lock. Requires Docker. The per-type retention sweep has its own
//! suite (`retention_sweep_integration_pg`).

mod common;

use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;

/// Concurrently-initializing replicas must not corrupt the post-migration
/// setup. The advisory lock serializes them, so every call succeeds, no
/// table-wide retention policy is left behind, and both configured intervals
/// are what the dimensions report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_concurrent_post_migration_setup_is_serialized() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let mut tasks = Vec::new();
    for _ in 0..8u32 {
        let pool = h.pool.clone();
        tasks.push(tokio::spawn(async move {
            apply_post_migration_setup(&pool, 604_800, 1).await
        }));
    }
    for t in tasks {
        t.await
            .expect("setup task did not panic")
            .expect("concurrent post-migration setup must succeed under the advisory lock");
    }

    let policies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("retention policy count");
    assert_eq!(
        policies, 0,
        "no table-wide retention policy may remain: retention is per type"
    );

    let dims: Vec<(String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT column_name::text, time_interval::text, integer_interval \
         FROM timescaledb_information.dimensions \
         WHERE hypertable_name = 'usage_records' ORDER BY dimension_number",
    )
    .fetch_all(&h.pool)
    .await
    .expect("dimension intervals");
    assert_eq!(
        dims,
        vec![
            ("window_end".to_owned(), Some("7 days".to_owned()), None),
            ("type_key".to_owned(), None, Some(1)),
        ],
        "the configured chunk interval and slice width must be applied"
    );
}

/// The init advisory lock must not leave a modified `statement_timeout` on any
/// pooled connection. `apply_post_migration_setup` acquires the lock on a pooled
/// connection; the wait is bounded by the connection-level GUC set in
/// `build_pool`, NOT by a per-lock session-level `SET` — a session-level set
/// would leak onto the connection and silently apply to whatever request later
/// reused it. With a distinct configured timeout (17s) and a fixed 2-connection
/// pool (the lock uses one connection, the setup statements a second), every
/// pooled connection must still report the configured value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_init_lock_does_not_leak_statement_timeout() {
    let h = common::bring_up_with(17, 2, 2)
        .await
        .expect("timescaledb container (Docker required)");

    let mut c1 = h.pool.acquire().await.expect("acquire conn 1");
    let mut c2 = h.pool.acquire().await.expect("acquire conn 2");
    let t1: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&mut *c1)
        .await
        .expect("SHOW statement_timeout on conn 1");
    let t2: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&mut *c2)
        .await
        .expect("SHOW statement_timeout on conn 2");

    assert_eq!(
        t1, "17s",
        "conn 1 carries a leaked statement_timeout; the init path must not set a \
         session-level statement_timeout on a pooled connection"
    );
    assert_eq!(
        t2, "17s",
        "conn 2 carries a leaked statement_timeout; the init path must not set a \
         session-level statement_timeout on a pooled connection"
    );
}
```

In `tests/schema_integration_pg.rs`, replace `a_retention_policy_is_registered_against_the_ledger` with:

```rust
/// Retention is per type and applied by the plugin's sweep, so no table-wide
/// `TimescaleDB` retention policy may be registered: one would drop every type
/// at a single horizon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_table_wide_retention_policy_is_registered() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("jobs query");
    assert_eq!(jobs, 0, "no table-wide retention policy may be registered");
}
```

- [ ] **Step 8: Update the README configuration**

In `README.md`, replace the configuration table and TOML example with:

```markdown
| Key | Default | Description |
| --- | --- | --- |
| `database_url` | _(required)_ | Postgres DSN; TLS required (use `sslmode=require`). |
| `pool_size_min` | `2` | Connection-pool lower bound. |
| `pool_size_max` | `16` | Connection-pool upper bound (at least 2). |
| `connection_timeout_secs` | `10` | Connection acquire timeout (seconds). |
| `statement_timeout_secs` | `30` | Per-statement timeout on every request-path connection (seconds). |
| `chunk_time_interval_secs` | `604800` (7d) | Time width of new ledger chunks; applies to chunks created afterwards. |
| `type_key_slice_width` | `1` | How many type keys share one chunk slice; applies to chunks created afterwards. See [Retention](#retention). |
| `retention_sweep_interval_secs` | `3600` (1h) | Seconds between retention sweeps. |
| `vendor` | `cyberfabric` | Vendor name for GTS instance registration. |
| `priority` | `10` | Plugin priority (lower = higher precedence). |

```toml
[gears.timescaledb-usage-collector-plugin.config]
database_url = "postgres://user:pass@host:5432/usage?sslmode=require"
pool_size_min = 2
pool_size_max = 16
connection_timeout_secs = 10
statement_timeout_secs = 30
chunk_time_interval_secs = 604800
type_key_slice_width = 1
retention_sweep_interval_secs = 3600
vendor = "cyberfabric"
priority = 10
```
```

In the Storage semantics list, replace the **Retention** bullet with: `- **Retention** — per GTS type, applied by the plugin's retention sweep; see [Retention](#retention).` Task 8 adds that section.

- [ ] **Step 9: Run everything touched**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS.

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres`
Expected: PASS.

`grep -rn "retention_period_secs\|NO_DROP_RETENTION_SECS\|bring_up_real_retention" gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin` must print only the rejection test in `config_tests.rs`.

- [ ] **Step 10: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "feat(timescaledb-plugin)!: replace the table-wide retention policy with partitioning config" \
  -m "BREAKING CHANGE: the retention_period_secs config key is removed and rejected. Retention is now declared per GTS type and applied by the plugin; chunk_time_interval_secs, type_key_slice_width and retention_sweep_interval_secs are new."
```

---

### Task 4: Read a type's declared retention from types-registry

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/domain/ports.rs`
- Create: `src/infra/registry_retention.rs`, `src/infra/registry_retention_tests.rs`
- Modify: `src/infra.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub enum RetentionError { Unavailable(String), NotFound, MissingTrait, InvalidTrait(String) }` (`Debug, Clone, PartialEq, Eq`).
  - `#[async_trait] pub trait RetentionSource: Send + Sync + 'static { async fn retention(&self, gts_type_id: &str) -> Result<std::time::Duration, RetentionError>; }`.
  - `pub struct TypesRegistryRetentionSource` with `pub fn new(hub: Arc<toolkit::client_hub::ClientHub>) -> Self`.
  - `pub fn retention_from_traits(traits: &serde_json::Value) -> Result<std::time::Duration, RetentionError>`.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`, under `[dependencies]` after `types-registry = { workspace = true }`:

```toml
# ISO 8601 duration parsing for the declared `retention` trait.
toolkit-utils = { workspace = true }
```

Under `[dev-dependencies]`:

```toml
# `MockTypesRegistryClient` for the retention-source unit tests.
types-registry-sdk = { workspace = true, features = ["test-util"] }
```

- [ ] **Step 2: Add the port**

Append to `src/domain/ports.rs`. Add `use std::time::Duration;` to its imports.

```rust
/// Why a type's declared retention could not be resolved.
///
/// Every variant keeps data: the retention sweep never drops a chunk without a
/// definite retention for each type it may hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionError {
    /// The registry client is missing, or the call failed with anything other
    /// than a definite not-found.
    Unavailable(String),
    /// The registry holds no such type.
    NotFound,
    /// The type declares no `retention` trait.
    MissingTrait,
    /// `retention` is not a string, not a fixed-length ISO 8601 duration, or
    /// zero.
    InvalidTrait(String),
}

/// Reads the declared retention of a GTS type.
///
/// Retention is the one declaration attribute this plugin reads, because it is
/// the component that applies it (the gear's `DESIGN.md` §3.3). It is mutable,
/// so an implementation must not cache it across sweeps.
#[async_trait]
pub trait RetentionSource: Send + Sync + 'static {
    async fn retention(&self, gts_type_id: &str) -> Result<Duration, RetentionError>;
}
```

- [ ] **Step 3: Write the failing tests**

Create `src/infra/registry_retention_tests.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use toolkit::client_hub::ClientHub;
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{GtsTypeId, GtsTypeSchema, TypesRegistryClient};

use super::{TypesRegistryRetentionSource, retention_from_traits};
use crate::domain::ports::{RetentionError, RetentionSource};

const BASE: &str = "gts.cf.core.uc.usage_record.v1~";
const METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";
const DAY: u64 = 86_400;

/// A base + derived chain, so the traits are read through
/// `GtsTypeSchema::effective_traits` exactly as production reads them.
fn meter_schema(traits: serde_json::Value) -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE).expect("base type id"),
        json!({
            "type": "object",
            "x-gts-abstract": true,
            "properties": {
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } }
            }
        }),
        None,
        None,
    )
    .expect("base schema");
    GtsTypeSchema::try_new(
        GtsTypeId::try_new(METER).expect("meter type id"),
        json!({
            "allOf": [ { "$ref": format!("gts://{BASE}") } ],
            "x-gts-traits": traits
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("meter schema")
}

fn source_over(mock: MockTypesRegistryClient) -> TypesRegistryRetentionSource {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn TypesRegistryClient>(Arc::new(mock));
    TypesRegistryRetentionSource::new(hub)
}

#[test]
fn a_day_count_parses_to_whole_days() {
    assert_eq!(
        retention_from_traits(&json!({ "retention": "P125D" })),
        Ok(Duration::from_secs(125 * DAY))
    );
}

#[test]
fn hours_are_a_fixed_length_and_are_accepted() {
    assert_eq!(
        retention_from_traits(&json!({ "retention": "PT36H" })),
        Ok(Duration::from_secs(36 * 3_600))
    );
}

#[test]
fn years_and_months_are_rejected_as_not_fixed_length() {
    for calendar in ["P1Y", "P1M"] {
        assert!(
            matches!(
                retention_from_traits(&json!({ "retention": calendar })),
                Err(RetentionError::InvalidTrait(_))
            ),
            "{calendar} is not a fixed number of seconds and must not be guessed at"
        );
    }
}

#[test]
fn a_zero_retention_is_rejected() {
    assert!(matches!(
        retention_from_traits(&json!({ "retention": "PT0S" })),
        Err(RetentionError::InvalidTrait(_))
    ));
}

#[test]
fn a_non_string_retention_is_rejected() {
    assert!(matches!(
        retention_from_traits(&json!({ "retention": 125 })),
        Err(RetentionError::InvalidTrait(_))
    ));
}

#[test]
fn an_absent_retention_is_reported_as_missing() {
    assert_eq!(
        retention_from_traits(&json!({ "aggregation_fold": "SUM" })),
        Err(RetentionError::MissingTrait)
    );
}

#[tokio::test]
async fn a_registered_type_resolves_through_its_effective_traits() {
    let source = source_over(MockTypesRegistryClient::new().with_type_schemas([meter_schema(
        json!({ "aggregation_fold": "SUM", "canonical_unit": "count", "retention": "P400D" }),
    )]));
    assert_eq!(
        source.retention(METER).await,
        Ok(Duration::from_secs(400 * DAY))
    );
}

#[tokio::test]
async fn an_unregistered_type_is_not_found() {
    let source = source_over(MockTypesRegistryClient::new());
    assert_eq!(source.retention(METER).await, Err(RetentionError::NotFound));
}

#[tokio::test]
async fn a_malformed_type_id_is_unavailable_rather_than_not_found() {
    let source = source_over(MockTypesRegistryClient::new());
    assert!(matches!(
        source.retention("not-a-type-id").await,
        Err(RetentionError::Unavailable(_))
    ));
}

#[tokio::test]
async fn a_missing_registry_client_is_unavailable() {
    let source = TypesRegistryRetentionSource::new(Arc::new(ClientHub::default()));
    assert!(matches!(
        source.retention(METER).await,
        Err(RetentionError::Unavailable(_))
    ));
}
```

- [ ] **Step 4: Run to verify they fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib registry_retention`

Expected: FAIL to compile, because `registry_retention` does not exist.

- [ ] **Step 5: Implement the source**

Create `src/infra/registry_retention.rs`:

```rust
//! [`RetentionSource`] over `types-registry`.
//!
//! The plugin reads a type's declared `retention` from the registry itself; no
//! SPI method carries it (the gear's ADR-0015 statement 6). It is read on every
//! sweep, never cached, because retention is not on the declaration's
//! immutable list (ADR-0008 statement 3).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use toolkit::client_hub::ClientHub;
use toolkit_utils::iso8601_duration::Iso8601Duration;
use types_registry_sdk::{TypesRegistryClient, TypesRegistryError};

use crate::domain::ports::{RetentionError, RetentionSource};

/// Resolves retention through the `TypesRegistryClient` in `ClientHub`.
pub struct TypesRegistryRetentionSource {
    hub: Arc<ClientHub>,
}

impl TypesRegistryRetentionSource {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl RetentionSource for TypesRegistryRetentionSource {
    async fn retention(&self, gts_type_id: &str) -> Result<Duration, RetentionError> {
        // Resolved per call rather than captured at construction: the registry
        // client may register after this gear's `init`.
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| RetentionError::Unavailable(e.to_string()))?;
        let schema = registry
            .get_type_schema(gts_type_id)
            .await
            .map_err(|e| classify(TypesRegistryError::from(e)))?;
        retention_from_traits(&schema.effective_traits())
    }
}

/// A definite not-found keeps its meaning; every other failure is an
/// availability problem, which the sweep treats the same way — it keeps data.
fn classify(err: TypesRegistryError) -> RetentionError {
    match err {
        TypesRegistryError::NotFound { .. } => RetentionError::NotFound,
        other => RetentionError::Unavailable(other.to_string()),
    }
}

/// The declared `retention` out of a type's merged traits.
///
/// # Errors
///
/// [`RetentionError::MissingTrait`] when the trait is absent, and
/// [`RetentionError::InvalidTrait`] when it is not a string, not a
/// fixed-length ISO 8601 duration (years and months are rejected by
/// [`Iso8601Duration`]), or zero.
pub fn retention_from_traits(traits: &Value) -> Result<Duration, RetentionError> {
    let raw = traits.get("retention").ok_or(RetentionError::MissingTrait)?;
    let text = raw
        .as_str()
        .ok_or_else(|| RetentionError::InvalidTrait(format!("retention is not a string: {raw}")))?;
    let parsed: Iso8601Duration = text
        .parse()
        .map_err(|e| RetentionError::InvalidTrait(format!("retention `{text}`: {e}")))?;
    let duration = parsed.as_duration();
    if duration.is_zero() {
        return Err(RetentionError::InvalidTrait(format!(
            "retention `{text}` is zero"
        )));
    }
    Ok(duration)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "registry_retention_tests.rs"]
mod registry_retention_tests;
```

In `src/infra.rs`, append:

```rust
#[doc(hidden)]
pub mod registry_retention;
```

- [ ] **Step 6: Run to verify they pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib registry_retention`

Expected: PASS (10 tests).

- [ ] **Step 7: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "feat(timescaledb-plugin): read a type's declared retention from types-registry"
```

---

### Task 5: Decide a chunk's expiry from the retentions of its types

**Files:**
- Create: `src/domain/retention.rs`, `src/domain/retention_tests.rs`
- Modify: `src/domain.rs`

**Interfaces:**
- Consumes: `RetentionError` from Task 4.
- Produces:
  - `pub enum KeepReason { NotExpired, NoType, Unavailable, NotFound, MissingTrait, InvalidTrait }` with `pub const fn is_unresolved(self) -> bool`, `pub const fn as_label(self) -> &'static str` and `impl From<&RetentionError> for KeepReason`.
  - `pub enum Decision { Drop, Keep(KeepReason) }`.
  - `pub fn drop_decision(time_end: time::OffsetDateTime, retentions: &[Result<std::time::Duration, RetentionError>], now: time::OffsetDateTime) -> Decision`.

- [ ] **Step 1: Write the failing tests**

Create `src/domain/retention_tests.rs`:

```rust
use std::time::Duration;

use time::OffsetDateTime;

use super::{Decision, KeepReason, drop_decision};
use crate::domain::ports::RetentionError;

const DAY: u64 = 86_400;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("valid instant")
}

fn days(n: u64) -> Result<Duration, RetentionError> {
    Ok(Duration::from_secs(n * DAY))
}

#[test]
fn a_chunk_past_its_types_retention_is_dropped() {
    let time_end = now() - time::Duration::days(100);
    assert_eq!(drop_decision(time_end, &[days(30)], now()), Decision::Drop);
}

#[test]
fn a_chunk_inside_its_types_retention_is_kept() {
    let time_end = now() - time::Duration::days(10);
    assert_eq!(
        drop_decision(time_end, &[days(30)], now()),
        Decision::Keep(KeepReason::NotExpired)
    );
}

#[test]
fn a_deadline_exactly_now_is_kept() {
    // The deadline is the first instant the retention no longer covers; the
    // chunk goes only once that instant is in the past.
    let time_end = now() - time::Duration::days(30);
    assert_eq!(
        drop_decision(time_end, &[days(30)], now()),
        Decision::Keep(KeepReason::NotExpired)
    );
}

#[test]
fn a_shared_chunk_is_held_to_its_longest_retention() {
    let time_end = now() - time::Duration::days(100);
    assert_eq!(
        drop_decision(time_end, &[days(30), days(400)], now()),
        Decision::Keep(KeepReason::NotExpired),
        "one type still inside its retention keeps the whole chunk"
    );
    assert_eq!(
        drop_decision(time_end, &[days(30), days(60)], now()),
        Decision::Drop
    );
}

#[test]
fn an_unresolved_type_keeps_a_chunk_its_neighbour_would_drop() {
    let time_end = now() - time::Duration::days(100);
    assert_eq!(
        drop_decision(
            time_end,
            &[days(1), Err(RetentionError::Unavailable("registry down".to_owned()))],
            now()
        ),
        Decision::Keep(KeepReason::Unavailable),
        "never drop without a definite retention for every type in the chunk"
    );
}

#[test]
fn a_chunk_no_type_maps_to_is_kept() {
    let time_end = now() - time::Duration::days(1_000);
    assert_eq!(
        drop_decision(time_end, &[], now()),
        Decision::Keep(KeepReason::NoType)
    );
}

#[test]
fn each_resolution_failure_keeps_under_its_own_reason() {
    let time_end = now() - time::Duration::days(1_000);
    for (err, reason) in [
        (RetentionError::Unavailable("x".to_owned()), KeepReason::Unavailable),
        (RetentionError::NotFound, KeepReason::NotFound),
        (RetentionError::MissingTrait, KeepReason::MissingTrait),
        (RetentionError::InvalidTrait("x".to_owned()), KeepReason::InvalidTrait),
    ] {
        assert_eq!(
            drop_decision(time_end, &[Err(err)], now()),
            Decision::Keep(reason)
        );
    }
}

#[test]
fn a_retention_too_long_to_add_to_an_instant_has_not_expired() {
    let time_end = now() - time::Duration::days(1_000);
    assert_eq!(
        drop_decision(time_end, &[Ok(Duration::MAX)], now()),
        Decision::Keep(KeepReason::NotExpired)
    );
}

#[test]
fn only_not_expired_is_a_resolved_keep() {
    assert!(!KeepReason::NotExpired.is_unresolved());
    for reason in [
        KeepReason::NoType,
        KeepReason::Unavailable,
        KeepReason::NotFound,
        KeepReason::MissingTrait,
        KeepReason::InvalidTrait,
    ] {
        assert!(reason.is_unresolved(), "{reason:?}");
    }
}

#[test]
fn unresolved_reasons_carry_the_metric_label_values() {
    assert_eq!(KeepReason::Unavailable.as_label(), "unavailable");
    assert_eq!(KeepReason::NotFound.as_label(), "not_found");
    assert_eq!(KeepReason::MissingTrait.as_label(), "missing_trait");
    assert_eq!(KeepReason::InvalidTrait.as_label(), "invalid_trait");
    assert_eq!(KeepReason::NoType.as_label(), "no_type");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib domain::retention`

Expected: FAIL to compile, because `retention` does not exist.

- [ ] **Step 3: Implement**

Create `src/domain/retention.rs`:

```rust
//! The retention sweep's decision for one ledger chunk.
//!
//! Pure: no database, no registry, no clock. The sweep supplies the chunk's
//! time range end, the retention of every type in its key range, and `now`.

use std::time::Duration;

use time::OffsetDateTime;

use crate::domain::ports::RetentionError;

/// Why a chunk is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// Every type resolved, and at least one is still inside its retention.
    NotExpired,
    /// No type maps to the chunk's key range.
    NoType,
    /// A type's retention could not be read from the registry.
    Unavailable,
    /// A type in the chunk is not registered.
    NotFound,
    /// A type in the chunk declares no retention.
    MissingTrait,
    /// A type in the chunk declares an unusable retention.
    InvalidTrait,
}

impl KeepReason {
    /// Whether the chunk is kept because a retention could not be resolved,
    /// rather than because it has not expired.
    #[must_use]
    pub const fn is_unresolved(self) -> bool {
        !matches!(self, Self::NotExpired)
    }

    /// The bounded label value for this reason.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::NotExpired => "not_expired",
            Self::NoType => "no_type",
            Self::Unavailable => "unavailable",
            Self::NotFound => "not_found",
            Self::MissingTrait => "missing_trait",
            Self::InvalidTrait => "invalid_trait",
        }
    }
}

impl From<&RetentionError> for KeepReason {
    fn from(err: &RetentionError) -> Self {
        match err {
            RetentionError::Unavailable(_) => Self::Unavailable,
            RetentionError::NotFound => Self::NotFound,
            RetentionError::MissingTrait => Self::MissingTrait,
            RetentionError::InvalidTrait(_) => Self::InvalidTrait,
        }
    }
}

/// What the sweep does with one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Drop,
    Keep(KeepReason),
}

/// Decide one chunk.
///
/// `time_end` is the exclusive end of the chunk's `window_end` range, so it
/// bounds every entry in the chunk from above: a deadline computed from it is
/// never earlier than any entry's own, and a drop never frees a dedup identity
/// before its horizon. `retentions` holds one resolution per type in the
/// chunk's key range.
///
/// The chunk is dropped only when every type resolved and the longest retention
/// has passed: `time_end + longest < now`. Any failed resolution keeps it, under
/// the first failure's reason.
#[must_use]
pub fn drop_decision(
    time_end: OffsetDateTime,
    retentions: &[Result<Duration, RetentionError>],
    now: OffsetDateTime,
) -> Decision {
    if retentions.is_empty() {
        return Decision::Keep(KeepReason::NoType);
    }
    let mut longest = Duration::ZERO;
    for retention in retentions {
        match retention {
            Ok(duration) => longest = longest.max(*duration),
            Err(err) => return Decision::Keep(KeepReason::from(err)),
        }
    }
    // A retention too long to add to an instant cannot have expired.
    let Some(deadline) = time::Duration::try_from(longest)
        .ok()
        .and_then(|duration| time_end.checked_add(duration))
    else {
        return Decision::Keep(KeepReason::NotExpired);
    };
    if deadline < now {
        Decision::Drop
    } else {
        Decision::Keep(KeepReason::NotExpired)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "retention_tests.rs"]
mod retention_tests;
```

In `src/domain.rs`, add `pub mod retention;`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib domain::retention`

Expected: PASS (10 tests).

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "feat(timescaledb-plugin): decide a chunk's expiry from the retentions of its types"
```

---

### Task 6: Sweep ledger chunks past their types' retention

**Files:**
- Modify: `src/infra/metrics.rs`, `src/infra/metrics_tests.rs`
- Create: `src/infra/storage/retention_sweep.rs`, `src/infra/storage/retention_sweep_tests.rs`
- Modify: `src/infra/storage.rs`
- Modify: `tests/schema_integration_pg.rs`
- Create: `tests/retention_sweep_integration_pg.rs`

**Interfaces:**
- Consumes:
  - `RetentionSource` and `RetentionError` (Task 4).
  - `drop_decision`, `Decision` and `KeepReason` (Task 5).
  - The `type_key` dimension and `usage_type_key` (Task 1).
  - `apply_post_migration_setup(pool, secs, width)` (Task 3).
- Produces:
  - `pub enum SweepOutcome { Completed, SkippedLocked, Failed }`.
  - `Metrics` methods: `record_retention_sweep(SweepOutcome, f64)`, `inc_retention_chunk_dropped()`, `inc_retention_chunk_kept_unresolved(KeepReason)`, `inc_retention_drop_failure()` and `set_chunks(u64)`.
  - In `retention_sweep`: `pub const SWEEP_ADVISORY_LOCK_KEY: i64`, `pub struct ChunkSlice { pub chunk: String, pub time_end: OffsetDateTime, pub key_start: i64, pub key_end: i64 }` and `pub async fn list_chunks(pool: &PgPool) -> Result<Vec<ChunkSlice>, sqlx::Error>`.
  - `pub struct SweepReport { pub skipped_locked: bool, pub chunks_seen: usize, pub dropped: usize, pub kept_unresolved: usize, pub drop_failures: usize }`.
  - `pub struct PgRetentionSweeper` with `pub fn new(pool: PgPool, source: Arc<dyn RetentionSource>, metrics: Arc<Metrics>) -> Self` and `pub async fn sweep_once(&self) -> Result<SweepReport, sqlx::Error>`.

- [ ] **Step 1: Write the failing metrics test**

In `src/infra/metrics_tests.rs`:
- Change the `use super::{…}` line to also import `SweepOutcome`, and add `use crate::domain::retention::KeepReason;`.
- In `every_exported_instrument_obeys_the_naming_convention`, directly after `metrics.set_ready(true);`, add:

```rust
    metrics.record_retention_sweep(SweepOutcome::Completed, 0.001);
    metrics.inc_retention_chunk_dropped();
    metrics.inc_retention_chunk_kept_unresolved(KeepReason::Unavailable);
    metrics.inc_retention_drop_failure();
    metrics.set_chunks(1);
```

- In that test's doc comment, change "it holds, at 18" to "it holds, at 24".

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib metrics_tests`

Expected: FAIL to compile, because `SweepOutcome` and the methods do not exist.

- [ ] **Step 2: Implement the instruments**

In `src/infra/metrics.rs`:

(a) In `pub mod label`, append:

```rust
    pub const SWEEP_OUTCOME: &str = "outcome";
    pub const SWEEP_OUTCOME_COMPLETED: &str = "completed";
    pub const SWEEP_OUTCOME_SKIPPED_LOCKED: &str = "skipped_locked";
    pub const SWEEP_OUTCOME_FAILED: &str = "failed";

    pub const KEEP_REASON: &str = "reason";
```

(b) After `ErrorClass`, add:

```rust
/// How one retention sweep ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepOutcome {
    /// The sweep held the lock and walked every chunk.
    Completed,
    /// Another replica held the sweep lock; nothing was read.
    SkippedLocked,
    /// A catalog or connection error ended the sweep.
    Failed,
}

impl SweepOutcome {
    const fn as_label(self) -> &'static str {
        match self {
            Self::Completed => label::SWEEP_OUTCOME_COMPLETED,
            Self::SkippedLocked => label::SWEEP_OUTCOME_SKIPPED_LOCKED,
            Self::Failed => label::SWEEP_OUTCOME_FAILED,
        }
    }
}
```

Add `use crate::domain::retention::KeepReason;` to the imports.

(c) In `struct Metrics`:
- after `batch_rows`, add `retention_sweep_duration: Histogram<f64>,`;
- after `tls_handshake_failure`, add `retention_sweeps: Counter<u64>, retention_chunks_dropped: Counter<u64>, retention_chunks_kept_unresolved: Counter<u64>, retention_drop_failures: Counter<u64>,` (one per line);
- after `ready`, add `chunks: Gauge<u64>,`.

(d) In `with_meter`, before `let ready = …`, build:

```rust
        let retention_sweep_duration = meter
            .f64_histogram("uc_timescaledb_retention_sweep_duration_seconds")
            .with_description("Duration of one retention sweep, whatever its outcome")
            .with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())
            .build();
        let retention_sweeps = meter
            .u64_counter("uc_timescaledb_retention_sweeps_total")
            .with_description(
                "Retention sweeps, by outcome: completed, skipped_locked (another replica \
                 held the sweep lock) or failed",
            )
            .build();
        let retention_chunks_dropped = meter
            .u64_counter("uc_timescaledb_retention_chunks_dropped_total")
            .with_description(
                "Ledger chunks dropped because every type in them had passed its declared \
                 retention",
            )
            .build();
        let retention_chunks_kept_unresolved = meter
            .u64_counter("uc_timescaledb_retention_chunks_kept_unresolved_total")
            .with_description(
                "Ledger chunks kept because a type in them had no resolvable retention, by \
                 reason; counted once per chunk per sweep, so a persistent cause grows by \
                 the chunk count every sweep",
            )
            .build();
        let retention_drop_failures = meter
            .u64_counter("uc_timescaledb_retention_drop_failures_total")
            .with_description("Expired ledger chunks whose drop failed; retried next sweep")
            .build();
        let chunks = meter
            .u64_gauge("uc_timescaledb_chunks")
            .with_description("Ledger chunks remaining after the last completed retention sweep")
            .build();
```

Add each of those six names to the `Self { … }` literal.

(e) Add the helpers after `inc_query_request`:

```rust
    /// Record one retention sweep: its duration, and its outcome.
    pub fn record_retention_sweep(&self, outcome: SweepOutcome, secs: f64) {
        self.retention_sweep_duration.record(secs, &[]);
        self.retention_sweeps
            .add(1, &[KeyValue::new(label::SWEEP_OUTCOME, outcome.as_label())]);
    }

    /// Increment the dropped-chunk counter.
    pub fn inc_retention_chunk_dropped(&self) {
        self.retention_chunks_dropped.add(1, &[]);
    }

    /// Increment the kept-unresolved counter under `reason`.
    pub fn inc_retention_chunk_kept_unresolved(&self, reason: KeepReason) {
        self.retention_chunks_kept_unresolved
            .add(1, &[KeyValue::new(label::KEEP_REASON, reason.as_label())]);
    }

    /// Increment the failed-drop counter.
    pub fn inc_retention_drop_failure(&self) {
        self.retention_drop_failures.add(1, &[]);
    }

    /// Set the chunk-count gauge.
    pub fn set_chunks(&self, n: u64) {
        self.chunks.record(n, &[]);
    }
```

(f) In `declared_instrument_names`:
- Add `retention_sweep_duration: _,`, `retention_sweeps: _,`, `retention_chunks_dropped: _,`, `retention_chunks_kept_unresolved: _,`, `retention_drop_failures: _,` and `chunks: _,` to the destructure.
- Add to the `vec!`:

```rust
            "uc_timescaledb_retention_sweep_duration_seconds",
            "uc_timescaledb_retention_sweeps_total",
            "uc_timescaledb_retention_chunks_dropped_total",
            "uc_timescaledb_retention_chunks_kept_unresolved_total",
            "uc_timescaledb_retention_drop_failures_total",
            "uc_timescaledb_chunks",
```

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib metrics_tests`

Expected: PASS.

- [ ] **Step 3: Write the failing catalog pin**

Append to `tests/schema_integration_pg.rs`. Add `use rust_decimal::Decimal;`, `use uuid::Uuid;`, `use timescaledb_usage_collector_plugin::domain::ports::RecordStore;` and `use timescaledb_usage_collector_plugin::infra::storage::retention_sweep;` to its imports.

```rust
/// The retention sweep reads chunk ranges out of `TimescaleDB`'s internal
/// catalog, whose shape changed between 2.17 and 2.29. This is the pin that
/// fails when an image upgrade reshapes it again: one row per chunk, each with
/// a time range end and a width-1 key range naming a mapped type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chunk_catalog_query_reads_one_row_per_chunk_with_both_ranges() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0xCA7A);
    store
        .create(common::entry(&common::meter(common::VCPU_METER), tenant, "vcpu", Decimal::ONE))
        .await
        .expect("create vcpu");
    store
        .create(common::entry(&common::meter(common::GB_METER), tenant, "gb", Decimal::ONE))
        .await
        .expect("create gb");

    let chunks = retention_sweep::list_chunks(&h.pool)
        .await
        .expect("the catalog query must run against this TimescaleDB image");
    let shown: i64 = sqlx::query_scalar("SELECT count(*) FROM show_chunks('usage_records')")
        .fetch_one(&h.pool)
        .await
        .expect("show_chunks");
    assert_eq!(shown, 2, "two types in one time range are two chunks");
    assert_eq!(
        i64::try_from(chunks.len()).unwrap(),
        shown,
        "one catalog row per chunk: {chunks:?}"
    );

    let keys: Vec<i32> = sqlx::query_scalar("SELECT type_key FROM usage_type_key")
        .fetch_all(&h.pool)
        .await
        .expect("mapped keys");
    for chunk in &chunks {
        assert_eq!(chunk.key_end - chunk.key_start, 1, "slice width 1: {chunk:?}");
        assert!(
            keys.iter().any(|k| i64::from(*k) == chunk.key_start),
            "each chunk's key range names a mapped type: {chunk:?} vs {keys:?}"
        );
        assert!(
            chunk.time_end > common::fixture_window_end(),
            "the time range end bounds every window_end in the chunk: {chunk:?}"
        );
    }
}
```

- [ ] **Step 4: Write the failing sweep integration suite**

Create `tests/retention_sweep_integration_pg.rs`:

```rust
#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The retention sweep against a live `TimescaleDB`, over a stub retention
//! source whose values a test can amend between sweeps. Requires Docker.
//!
//! Fixtures are written through the real ingest path with a covered period
//! that ended a given number of days ago; the default 7-day chunk interval puts
//! entries of one type and one age into one chunk.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use usage_collector_sdk::UsageRecord;

use timescaledb_usage_collector_plugin::domain::ports::{
    RecordStore, RetentionError, RetentionSource,
};
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;
use timescaledb_usage_collector_plugin::infra::storage::retention_sweep::{
    PgRetentionSweeper, SWEEP_ADVISORY_LOCK_KEY,
};

const DAY: u64 = 86_400;

/// Retention per type, amendable mid-test. A type with no entry is not found.
#[derive(Default)]
struct StubRetention {
    by_type: Mutex<HashMap<String, Result<StdDuration, RetentionError>>>,
}

impl StubRetention {
    fn set(&self, gts_type_id: &str, retention: Result<StdDuration, RetentionError>) {
        self.by_type
            .lock()
            .unwrap()
            .insert(gts_type_id.to_owned(), retention);
    }
}

#[async_trait]
impl RetentionSource for StubRetention {
    async fn retention(&self, gts_type_id: &str) -> Result<StdDuration, RetentionError> {
        self.by_type
            .lock()
            .unwrap()
            .get(gts_type_id)
            .cloned()
            .unwrap_or(Err(RetentionError::NotFound))
    }
}

fn days(n: u64) -> Result<StdDuration, RetentionError> {
    Ok(StdDuration::from_secs(n * DAY))
}

/// An entry of `meter` whose covered period ended `days_ago` days ago.
fn aged(meter: &str, tenant: Uuid, idem: &str, days_ago: i64) -> UsageRecord {
    let end = OffsetDateTime::now_utc() - Duration::days(days_ago);
    common::entry_over(
        &common::meter(meter),
        tenant,
        idem,
        Decimal::ONE,
        end - Duration::hours(1),
        end,
    )
}

async fn stored(pool: &sqlx::PgPool, id: Uuid) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count by id");
    n == 1
}

fn sweeper(pool: &sqlx::PgPool, stub: &Arc<StubRetention>) -> PgRetentionSweeper {
    PgRetentionSweeper::new(
        pool.clone(),
        Arc::clone(stub) as Arc<dyn RetentionSource>,
        Arc::new(Metrics::new(pool.clone())),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_type_expires_at_its_own_retention() {
    let h = common::bring_up().await.expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E01);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    stub.set(common::GB_METER, days(400));

    let old_vcpu = aged(common::VCPU_METER, tenant, "old-vcpu", 100);
    let old_gb = aged(common::GB_METER, tenant, "old-gb", 100);
    let new_vcpu = aged(common::VCPU_METER, tenant, "new-vcpu", 0);
    let (old_vcpu_id, old_gb_id, new_vcpu_id) = (old_vcpu.id, old_gb.id, new_vcpu.id);
    for entry in [old_vcpu, old_gb, new_vcpu] {
        store.create(entry).await.expect("create fixture");
    }

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!(report.dropped, 1, "{report:?}");
    assert!(!stored(&h.pool, old_vcpu_id).await, "vcpu past 30 days is dropped");
    assert!(stored(&h.pool, old_gb_id).await, "gb inside 400 days survives beside it");
    assert!(stored(&h.pool, new_vcpu_id).await, "a current vcpu period survives");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_amended_retention_applies_to_entries_already_stored() {
    let h = common::bring_up().await.expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    let entry = aged(common::VCPU_METER, Uuid::from_u128(0x5E02), "amended", 100);
    let id = entry.id;
    store.create(entry).await.expect("create fixture");
    let sweeper = sweeper(&h.pool, &stub);

    stub.set(common::VCPU_METER, days(400));
    let raised = sweeper.sweep_once().await.expect("sweep after raise");
    assert_eq!(raised.dropped, 0, "{raised:?}");
    assert!(stored(&h.pool, id).await, "a raised retention keeps an entry already stored");

    stub.set(common::VCPU_METER, days(30));
    let lowered = sweeper.sweep_once().await.expect("sweep after lower");
    assert_eq!(lowered.dropped, 1, "{lowered:?}");
    assert!(!stored(&h.pool, id).await, "a lowered retention drops an entry already stored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unresolvable_type_keeps_its_chunks_while_others_still_drop() {
    let h = common::bring_up().await.expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E03);
    let stub = Arc::new(StubRetention::default());
    stub.set(
        common::VCPU_METER,
        Err(RetentionError::Unavailable("registry down".to_owned())),
    );
    stub.set(common::GB_METER, days(30));

    let vcpu = aged(common::VCPU_METER, tenant, "vcpu", 100);
    let gb = aged(common::GB_METER, tenant, "gb", 100);
    let (vcpu_id, gb_id) = (vcpu.id, gb.id);
    store.create(vcpu).await.expect("create vcpu");
    store.create(gb).await.expect("create gb");

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!((report.dropped, report.kept_unresolved), (1, 1), "{report:?}");
    assert!(stored(&h.pool, vcpu_id).await, "no definite retention, no drop");
    assert!(!stored(&h.pool, gb_id).await, "a resolvable expired type still drops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_slice_is_held_to_its_longest_retention() {
    let h = common::bring_up().await.expect("timescaledb container (Docker required)");
    // Before any write, so the first chunks are created at width 4: keys 1 and
    // 2 share the slice [0, 4).
    apply_post_migration_setup(&h.pool, 604_800, 4)
        .await
        .expect("widen the type-key slice");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E04);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    stub.set(common::GB_METER, days(400));

    let vcpu = aged(common::VCPU_METER, tenant, "vcpu", 100);
    let gb = aged(common::GB_METER, tenant, "gb", 100);
    let (vcpu_id, gb_id) = (vcpu.id, gb.id);
    store.create(vcpu).await.expect("create vcpu");
    store.create(gb).await.expect("create gb");
    let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM show_chunks('usage_records')")
        .fetch_one(&h.pool)
        .await
        .expect("show_chunks");
    assert_eq!(chunks, 1, "both types share one chunk at width 4");
    let sweeper = sweeper(&h.pool, &stub);

    let held = sweeper.sweep_once().await.expect("first sweep");
    assert_eq!(held.dropped, 0, "{held:?}");
    assert!(stored(&h.pool, vcpu_id).await, "held to gb's 400 days, not vcpu's 30");

    stub.set(common::GB_METER, days(30));
    let released = sweeper.sweep_once().await.expect("second sweep");
    assert_eq!(released.dropped, 1, "{released:?}");
    assert!(!stored(&h.pool, vcpu_id).await && !stored(&h.pool, gb_id).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalidation_and_its_target_drop_together() {
    let h = common::bring_up().await.expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));

    let target = aged(common::VCPU_METER, Uuid::from_u128(0x5E05), "target", 100);
    let withdrawal = common::withdrawal_of(&target, "withdrawal");
    let (target_id, withdrawal_id) = (target.id, withdrawal.id);
    store.create(target).await.expect("create target");
    store.create(withdrawal).await.expect("create withdrawal");

    sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert!(!stored(&h.pool, target_id).await, "the target is dropped");
    assert!(!stored(&h.pool, withdrawal_id).await, "its invalidation goes with it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_into_a_dropped_range_is_collected_by_the_next_sweep() {
    let h = common::bring_up().await.expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E06);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    let sweeper = sweeper(&h.pool, &stub);

    store
        .create(aged(common::VCPU_METER, tenant, "first", 100))
        .await
        .expect("create first");
    assert_eq!(sweeper.sweep_once().await.expect("sweep 1").dropped, 1);

    let late = aged(common::VCPU_METER, tenant, "late", 100);
    let late_id = late.id;
    store.create(late).await.expect("a write into a dropped range recreates its chunk");
    assert!(stored(&h.pool, late_id).await);

    assert_eq!(sweeper.sweep_once().await.expect("sweep 2").dropped, 1);
    assert!(!stored(&h.pool, late_id).await, "the recreated chunk is collected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sweep_skips_while_another_session_holds_the_lock() {
    let h = common::bring_up().await.expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    let entry = aged(common::VCPU_METER, Uuid::from_u128(0x5E07), "locked", 100);
    let id = entry.id;
    store.create(entry).await.expect("create fixture");
    let sweeper = sweeper(&h.pool, &stub);

    // A different session: the advisory lock is re-entrant within one.
    let mut holder = h.pool.acquire().await.expect("acquire lock holder");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SWEEP_ADVISORY_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .expect("take the sweep lock");

    let skipped = sweeper.sweep_once().await.expect("sweep while locked");
    assert!(skipped.skipped_locked, "{skipped:?}");
    assert!(stored(&h.pool, id).await, "a skipped sweep drops nothing");

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SWEEP_ADVISORY_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .expect("release the sweep lock");
    let ran = sweeper.sweep_once().await.expect("sweep after release");
    assert!(!ran.skipped_locked && ran.dropped == 1, "{ran:?}");
}
```

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test retention_sweep_integration_pg --test schema_integration_pg`

Expected: FAIL to compile, because `retention_sweep` does not exist.

- [ ] **Step 5: Implement the sweeper**

Create `src/infra/storage/retention_sweep.rs`:

```rust
//! The retention sweep: drops every ledger chunk whose types have all passed
//! their declared retention.
//!
//! Retention lives in `types-registry`, which the database cannot reach, so the
//! sweep runs in the plugin rather than as a `TimescaleDB` job. It reads each
//! chunk's time and type-key range from the `TimescaleDB` catalog, resolves the
//! current retention of every type in that key range, and decides the chunk
//! through [`drop_decision`] — which never drops without a definite retention
//! for every type the chunk may hold.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (see
// `record_store.rs`).
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::{Connection as _, PgPool};
use time::OffsetDateTime;

use crate::domain::ports::{RetentionError, RetentionSource};
use crate::domain::retention::{Decision, drop_decision};
use crate::infra::metrics::{Metrics, SweepOutcome};

/// Advisory-lock key admitting one sweeper at a time across replicas.
/// Arbitrary but stable, and distinct from the init lock's key.
/// (`0x7563_7473` == ASCII `"ucts"`.)
pub const SWEEP_ADVISORY_LOCK_KEY: i64 = 0x7563_7473;

/// Every chunk of `usage_records` with its time range end and type-key range.
///
/// Reads internal catalog tables, whose shape changed between `TimescaleDB`
/// 2.17 and 2.29; `tests/schema_integration_pg.rs` pins it against the image
/// the plugin is tested on. Each chunk collapses to one row **before** the
/// time conversion, so no planner order can apply the conversion to the key
/// dimension's range.
pub const LIST_CHUNKS_SQL: &str = "SELECT ch.relid::text AS chunk, \
     _timescaledb_functions.to_timestamp(\
     max(ds.range_end) FILTER (WHERE d.column_name = 'window_end')) AS time_end, \
     max(ds.range_start) FILTER (WHERE d.column_name = 'type_key') AS key_start, \
     max(ds.range_end) FILTER (WHERE d.column_name = 'type_key') AS key_end \
     FROM _timescaledb_catalog.chunk ch \
     JOIN _timescaledb_catalog.hypertable h ON h.id = ch.hypertable_id \
     JOIN _timescaledb_catalog.dimension_slice ds ON ds.chunk_id = ch.id \
     JOIN _timescaledb_catalog.dimension d ON d.id = ds.dimension_id \
     WHERE h.table_name = 'usage_records' \
     GROUP BY ch.relid";

/// Drops one chunk by its schema-qualified name. `DROP TABLE <chunk>` is an
/// equivalent fallback should this internal function change.
pub const DROP_CHUNK_SQL: &str = "SELECT _timescaledb_functions.drop_chunk($1::regclass)";

/// One ledger chunk, as the `TimescaleDB` catalog describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSlice {
    /// Schema-qualified chunk table, e.g. `_timescaledb_internal._hyper_1_1_chunk`.
    pub chunk: String,
    /// Exclusive end of the chunk's `window_end` range.
    pub time_end: OffsetDateTime,
    /// Inclusive start of the chunk's `type_key` range.
    pub key_start: i64,
    /// Exclusive end of the chunk's `type_key` range.
    pub key_end: i64,
}

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Another replica held the sweep lock, so nothing was read.
    pub skipped_locked: bool,
    pub chunks_seen: usize,
    pub dropped: usize,
    pub kept_unresolved: usize,
    pub drop_failures: usize,
}

/// Every chunk of the ledger. A chunk missing either range — which a
/// two-dimension hypertable does not produce — is logged and left out, and so
/// is never dropped.
///
/// # Errors
///
/// Returns the `sqlx` error of the catalog query.
pub async fn list_chunks(pool: &PgPool) -> Result<Vec<ChunkSlice>, sqlx::Error> {
    let rows: Vec<(String, Option<OffsetDateTime>, Option<i64>, Option<i64>)> =
        sqlx::query_as(LIST_CHUNKS_SQL).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(chunk, time_end, key_start, key_end)| {
            if let (Some(time_end), Some(key_start), Some(key_end)) = (time_end, key_start, key_end)
            {
                Some(ChunkSlice {
                    chunk,
                    time_end,
                    key_start,
                    key_end,
                })
            } else {
                tracing::warn!(
                    chunk = %chunk,
                    "a ledger chunk lacks a window_end or type_key range; it is kept"
                );
                None
            }
        })
        .collect())
}

/// Runs retention sweeps over one database.
pub struct PgRetentionSweeper {
    pool: PgPool,
    source: Arc<dyn RetentionSource>,
    metrics: Arc<Metrics>,
}

impl PgRetentionSweeper {
    #[must_use]
    pub fn new(pool: PgPool, source: Arc<dyn RetentionSource>, metrics: Arc<Metrics>) -> Self {
        Self {
            pool,
            source,
            metrics,
        }
    }

    /// One sweep, recorded under its outcome whatever that is.
    ///
    /// # Errors
    ///
    /// Returns the `sqlx` error that ended the sweep: acquiring the lock
    /// connection, taking the lock, listing chunks or loading type keys. A
    /// failed drop does not end the sweep; it is counted in the report.
    pub async fn sweep_once(&self) -> Result<SweepReport, sqlx::Error> {
        let started = Instant::now();
        let result = self.sweep_under_lock().await;
        let outcome = match &result {
            Ok(report) if report.skipped_locked => SweepOutcome::SkippedLocked,
            Ok(_) => SweepOutcome::Completed,
            Err(_) => SweepOutcome::Failed,
        };
        self.metrics
            .record_retention_sweep(outcome, started.elapsed().as_secs_f64());
        result
    }

    /// Take the sweep lock and sweep, or report a skip.
    ///
    /// The lock is session-level and taken on a **detached** connection, so it
    /// lives exactly as long as that connection: closing it releases the lock
    /// on every path, including a sweep abandoned mid-way, and a locked
    /// connection is never handed back to the pool.
    async fn sweep_under_lock(&self) -> Result<SweepReport, sqlx::Error> {
        let mut lock_conn = self.pool.acquire().await?.detach();
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(SWEEP_ADVISORY_LOCK_KEY)
            .fetch_one(&mut lock_conn)
            .await?;
        let result = if locked {
            self.sweep(OffsetDateTime::now_utc()).await
        } else {
            Ok(SweepReport {
                skipped_locked: true,
                ..SweepReport::default()
            })
        };
        if let Err(e) = lock_conn.close().await {
            tracing::warn!(
                error = %e,
                "closing the retention sweep's lock connection failed; the lock frees with the session"
            );
        }
        result
    }

    /// One pass over every chunk, deciding each against the retentions of the
    /// types in its key range.
    async fn sweep(&self, now: OffsetDateTime) -> Result<SweepReport, sqlx::Error> {
        let chunks = list_chunks(&self.pool).await?;
        let type_keys: BTreeMap<i64, String> =
            sqlx::query_as::<_, (i32, String)>("SELECT type_key, gts_type_id FROM usage_type_key")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|(key, gts_type_id)| (i64::from(key), gts_type_id))
                .collect();

        let mut report = SweepReport {
            chunks_seen: chunks.len(),
            ..SweepReport::default()
        };
        // Resolved at most once per sweep and never across sweeps: retention is
        // mutable, and an amendment must reach the next sweep.
        let mut resolved: HashMap<String, Result<Duration, RetentionError>> = HashMap::new();

        for chunk in &chunks {
            let mut retentions = Vec::new();
            for gts_type_id in type_keys.range(chunk.key_start..chunk.key_end).map(|(_, id)| id) {
                let retention = if let Some(known) = resolved.get(gts_type_id) {
                    known.clone()
                } else {
                    let fresh = self.source.retention(gts_type_id).await;
                    resolved.insert(gts_type_id.clone(), fresh.clone());
                    fresh
                };
                retentions.push(retention);
            }

            match drop_decision(chunk.time_end, &retentions, now) {
                Decision::Drop => {
                    match sqlx::query(DROP_CHUNK_SQL)
                        .bind(&chunk.chunk)
                        .execute(&self.pool)
                        .await
                    {
                        Ok(_) => {
                            report.dropped += 1;
                            self.metrics.inc_retention_chunk_dropped();
                        }
                        Err(e) => {
                            report.drop_failures += 1;
                            self.metrics.inc_retention_drop_failure();
                            tracing::warn!(
                                chunk = %chunk.chunk,
                                error = %e,
                                "dropping an expired ledger chunk failed; the next sweep retries it"
                            );
                        }
                    }
                }
                Decision::Keep(reason) if reason.is_unresolved() => {
                    report.kept_unresolved += 1;
                    self.metrics.inc_retention_chunk_kept_unresolved(reason);
                    tracing::warn!(
                        chunk = %chunk.chunk,
                        reason = reason.as_label(),
                        "kept a ledger chunk whose retention could not be resolved"
                    );
                }
                Decision::Keep(_) => {}
            }
        }

        let remaining = chunks.len().saturating_sub(report.dropped);
        self.metrics
            .set_chunks(u64::try_from(remaining).unwrap_or(u64::MAX));
        Ok(report)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "retention_sweep_tests.rs"]
mod retention_sweep_tests;
```

Create `src/infra/storage/retention_sweep_tests.rs`:

```rust
use super::{DROP_CHUNK_SQL, LIST_CHUNKS_SQL, SWEEP_ADVISORY_LOCK_KEY};

#[test]
fn the_catalog_query_collapses_each_chunk_before_converting_its_time() {
    // Chaining the two dimension ranges without collapsing lets the planner
    // convert the key range as a timestamp (`TIMESCALEDB-RETENTION.md` §8.1).
    assert!(LIST_CHUNKS_SQL.contains("FILTER (WHERE d.column_name = 'window_end')"));
    assert!(LIST_CHUNKS_SQL.contains("FILTER (WHERE d.column_name = 'type_key')"));
    assert!(LIST_CHUNKS_SQL.ends_with("GROUP BY ch.relid"));
    assert!(LIST_CHUNKS_SQL.contains("WHERE h.table_name = 'usage_records'"));
}

#[test]
fn a_chunk_is_dropped_by_its_regclass() {
    assert_eq!(
        DROP_CHUNK_SQL,
        "SELECT _timescaledb_functions.drop_chunk($1::regclass)"
    );
}

#[test]
fn the_sweep_lock_is_not_the_init_lock() {
    // `pool::INIT_ADVISORY_LOCK_KEY` is `0x7563_7462`; sharing it would make a
    // sweep and a replica's startup setup block each other.
    assert_ne!(SWEEP_ADVISORY_LOCK_KEY, 0x7563_7462);
}
```

In `src/infra/storage.rs`, add `pub mod retention_sweep;` after `pub mod record_store;`.

- [ ] **Step 6: Run the unit tests and both Docker suites**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib`
Expected: PASS.

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres --test retention_sweep_integration_pg --test schema_integration_pg`
Expected: PASS (7 sweep tests, and the schema suite including the catalog pin).

- [ ] **Step 7: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "feat(timescaledb-plugin): sweep ledger chunks past their types' retention"
```

---

### Task 7: Run the retention sweep as the plugin's background task

**Files:**
- Modify: `src/gear.rs`
- Modify: `src/gear_tests.rs`

**Interfaces:**
- Consumes:
  - `TypesRegistryRetentionSource::new(Arc<ClientHub>)` (Task 4).
  - `PgRetentionSweeper::new` and `sweep_once` (Task 6).
  - `cfg.retention_sweep_interval_secs` (Task 3).
- Produces: `TimescaleDbUsageCollectorPlugin` implements `toolkit::contracts::RunnableCapability`. Its gear attribute declares `capabilities = [stateful]`.

- [ ] **Step 1: Write the failing lifecycle tests**

In `src/gear_tests.rs`:
- Add `use toolkit::contracts::RunnableCapability;`.
- Change `TimescaleDbUsageCollectorPlugin\n        .init(&ctx)` to `TimescaleDbUsageCollectorPlugin::default()\n        .init(&ctx)`.
- Append:

```rust
#[tokio::test]
async fn start_before_init_is_refused() {
    let plugin = TimescaleDbUsageCollectorPlugin::default();
    let err = plugin
        .start(CancellationToken::new())
        .await
        .expect_err("start must refuse to run a sweep init never built");
    assert!(
        err.to_string().contains("init() must run before start()"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn stop_without_start_is_a_no_op() {
    TimescaleDbUsageCollectorPlugin::default()
        .stop(CancellationToken::new())
        .await
        .expect("stopping a plugin that never started succeeds");
}
```

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib gear_tests`

Expected: FAIL to compile. The plugin is a unit struct, so `default()` is still callable, but it does not implement `RunnableCapability`.

- [ ] **Step 2: Implement the lifecycle**

In `src/gear.rs`:

(a) Imports. Replace `use std::sync::Arc;` with `use std::sync::{Arc, Mutex, OnceLock};` and add:

```rust
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use toolkit::contracts::RunnableCapability;
use toolkit::tokio::task::JoinHandle;

use crate::infra::registry_retention::TypesRegistryRetentionSource;
use crate::infra::storage::retention_sweep::PgRetentionSweeper;
```

(b) The gear attribute and struct:

```rust
#[toolkit::gear(
    name = "timescaledb-usage-collector-plugin",
    deps = [types_registry],
    capabilities = [stateful]
)]
#[derive(Default)]
pub struct TimescaleDbUsageCollectorPlugin {
    /// Built by `init`, run by `start`.
    sweep: OnceLock<SweepWiring>,
    sweep_cancel: Mutex<Option<CancellationToken>>,
    sweep_handle: Mutex<Option<JoinHandle<()>>>,
}

/// What `start` needs from `init` to run the retention sweep.
struct SweepWiring {
    sweeper: Arc<PgRetentionSweeper>,
    interval: Duration,
}
```

Extend the struct's doc comment with: "It also runs the per-type retention sweep as its background task (`RunnableCapability`)."

(c) In `init`, directly before `let record: Arc<dyn RecordStore> = Arc::new(PgRecordStore::new(`, insert:

```rust
        // The retention sweep reads each type's declared retention from
        // types-registry itself — the one declaration attribute this plugin
        // reads, because it is the component that applies it.
        let retention = Arc::new(TypesRegistryRetentionSource::new(ctx.client_hub()));
        let sweeper = Arc::new(PgRetentionSweeper::new(
            pool.clone(),
            retention,
            Arc::clone(&metrics),
        ));
        self.sweep
            .set(SweepWiring {
                sweeper,
                interval: Duration::from_secs(cfg.retention_sweep_interval_secs),
            })
            .map_err(|_| anyhow::anyhow!("timescaledb plugin init ran twice"))?;
```

(d) After the `impl Gear` block, add:

```rust
#[async_trait]
impl RunnableCapability for TimescaleDbUsageCollectorPlugin {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let wiring = self.sweep.get().ok_or_else(|| {
            anyhow::anyhow!("retention sweep not initialized - init() must run before start()")
        })?;
        let sweeper = Arc::clone(&wiring.sweeper);
        let interval = wiring.interval;
        let token = cancel.child_token();
        {
            let mut guard = self
                .sweep_cancel
                .lock()
                .map_err(|e| anyhow::anyhow!("sweep_cancel lock: {e}"))?;
            if guard.is_some() {
                anyhow::bail!("retention sweep already started");
            }
            *guard = Some(token.clone());
        }
        let handle = toolkit::tokio::spawn(run_sweeps(sweeper, interval, token));
        *self
            .sweep_handle
            .lock()
            .map_err(|e| anyhow::anyhow!("sweep_handle lock: {e}"))? = Some(handle);
        info!(interval_secs = interval.as_secs(), "retention sweep started");
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        if let Some(token) = self
            .sweep_cancel
            .lock()
            .map_err(|e| anyhow::anyhow!("sweep_cancel lock: {e}"))?
            .take()
        {
            token.cancel();
        }
        let handle = self
            .sweep_handle
            .lock()
            .map_err(|e| anyhow::anyhow!("sweep_handle lock: {e}"))?
            .take();
        if let Some(handle) = handle {
            toolkit::tokio::select! {
                result = handle => {
                    if let Err(e) = result
                        && !e.is_cancelled()
                    {
                        tracing::warn!(error = ?e, "retention sweep task failed");
                    }
                }
                () = deadline.cancelled() => {
                    tracing::info!("retention sweep stop cut short by the framework deadline");
                }
            }
        }
        Ok(())
    }
}

/// Sweep now, then once per `interval`, until `cancel` fires.
///
/// Cancellation is observed between sweeps only. A sweep holds the sweep lock
/// and may be part-way through a chunk drop; letting it finish is cheaper than
/// reasoning about where it stopped, and its lock connection closes either way.
async fn run_sweeps(sweeper: Arc<PgRetentionSweeper>, interval: Duration, cancel: CancellationToken) {
    loop {
        match sweeper.sweep_once().await {
            Ok(report) => tracing::debug!(?report, "retention sweep finished"),
            Err(e) => tracing::warn!(error = %e, "retention sweep failed; retrying next interval"),
        }
        toolkit::tokio::select! {
            () = cancel.cancelled() => break,
            () = toolkit::tokio::time::sleep(interval) => {}
        }
    }
}
```

- [ ] **Step 3: Run to verify they pass**

Run: `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --lib gear_tests`

Expected: PASS (3 tests).

- [ ] **Step 4: Build what links the plugin**

Run: `cargo check -p cf-gears-example-server --features timescaledb-usage-collector`

Expected: success. `apps/cf-gears-example-server` links the plugin behind that feature and resolves it through the gear inventory, so this confirms the `stateful` capability wiring compiles where the plugin is linked.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo fmt -p cf-gears-timescaledb-usage-collector-plugin
cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin
git commit -m "feat(timescaledb-plugin): run the retention sweep as the plugin's background task"
```

---

### Task 8: Document the enforced retention

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: the behaviour built in Tasks 1–7.
- Produces: the deployment-guide statement required by the gear's `DESIGN.md` §3.10 item 5.

- [ ] **Step 1: Update the storage-semantics bullets**

In `README.md`, **Deduplication** bullet:
- Replace `UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end)` with `UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end, type_key)`.
- Directly after the parenthetical file reference, add: "`type_key` is there because a hypertable UNIQUE must carry every partition column; a type's key never changes, so the dedup identity is still the 5-tuple".

**Invalidation** bullet: replace `over `(invalidates, window_end)`` with `over `(invalidates, window_end, type_key)``.

- [ ] **Step 2: Add the Retention section**

Insert after the Storage semantics section, before `## SPI conformance`:

```markdown
## Retention

This section is the plugin's deployment-guide statement of the retention it enforces per GTS type (the gear's `DESIGN.md` §3.10 item 5).

- **What is enforced.** Each type's **current** declared `retention` trait, read from `types-registry`, measured from the end of the covered period (`window_end`). The value must be a fixed-length ISO 8601 duration (`P125D`, `PT36H`); years and months are rejected.
- **How.** `usage_records` is partitioned on `window_end` and on a per-type integer `type_key` (`usage_type_key`). A background retention sweep runs every `retention_sweep_interval_secs` and drops a chunk once every type in its key range has passed its retention. There is no table-wide TimescaleDB retention policy; startup removes one if an earlier build left it.
- **Amendments.** A changed retention applies to entries already stored, in both directions, from the next sweep.
- **Over-retention.** A chunk is dropped whole, so an entry can be held up to one `chunk_time_interval_secs` past its retention. With `type_key_slice_width` above 1, types share chunks and a shared chunk is held to the longest retention among them. Both are permitted: a purge may run later than the horizon, never earlier.
- **Unresolvable retention keeps data.** If the registry is unreachable, a type is not registered, or its `retention` is missing or invalid, the chunks holding it are kept and `uc_timescaledb_retention_chunks_kept_unresolved_total` grows under the matching `reason`. That counter is the signal to act on.
- **Chunk count.** Roughly *(types ÷ `type_key_slice_width`)* × *(weeks retained at a 7-day interval)*, reported by `uc_timescaledb_chunks`. Keep it below 10 000: before it gets there, raise `type_key_slice_width` or `chunk_time_interval_secs`. Both apply to chunks created afterwards, so no data migration is needed.
- **The floor is the deployer's.** A meter a charging consumer reads must declare at least the retention floor (backfill window plus one replay horizon, 125 days at the launch defaults; `cpt-cf-usage-collector-fr-billing-retention-floor`). The plugin cannot tell which meters those are and does not check it.

Sweep metrics: `uc_timescaledb_retention_sweeps_total{outcome}`, `uc_timescaledb_retention_sweep_duration_seconds`, `uc_timescaledb_retention_chunks_dropped_total`, `uc_timescaledb_retention_chunks_kept_unresolved_total{reason}`, `uc_timescaledb_retention_drop_failures_total`, `uc_timescaledb_chunks`.
```

- [ ] **Step 3: Verify the whole crate**

Run each and confirm the stated result:
- `cargo fmt -p cf-gears-timescaledb-usage-collector-plugin --check` — no diff.
- `cargo clippy -p cf-gears-timescaledb-usage-collector-plugin --all-targets --all-features -- -D warnings` — no warnings.
- `cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres` — all pass.
- `grep -rn "retention_period_secs\|add_retention_policy\|dedup-cleanup job" gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/src gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/tests gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/README.md` — prints only the rejection test in `config_tests.rs`.

- [ ] **Step 4: Commit**

```bash
git add gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin/README.md
git commit -m "docs(timescaledb-plugin): state the retention the plugin enforces per type"
```
