# TimescaleDB plugin — per-type retention

**Date**: 2026-09-14
**Status**: Approved
**Scope**: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`

## 1. Why

`docs/DESIGN.md` makes retention a property of each GTS type, and makes the
storage plugin responsible for enforcing it:

- The base schema requires the `retention` trait, an ISO 8601 duration measured
  from the covered period (`docs/schemas/usage_record.v1.schema.json`).
- "Retention is declared per type, and the plugin reads it from `types-registry`
  itself. No gear surface carries it." (`DESIGN.md` driver table,
  `cpt-cf-usage-collector-fr-billing-retention-floor`)
- "Retention is not on that immutable list … An amended retention therefore
  reaches the only component that applies it." (ADR-0008 statement 3)
- "A purge never frees a dedup identity earlier than its horizon. A purge later
  than the horizon is permitted." (`DESIGN.md` §3.1, plugin-owned lifecycle)
- The deployment guide must state "the retention it enforces per GTS type"
  (`DESIGN.md` §3.10 item 5).

The plugin currently enforces one retention for the whole table. This spec
replaces it with per-type retention.

Rollups are a separate design cycle. See [§9](#9-out-of-scope).

### Documents that are current, and documents that are stale

These documents are current:

- `docs/DESIGN.md`
- `docs/PRD.md`
- the ADR set
- `docs/schemas/*`

These are stale, and must not be used as a basis for implementation:

- `docs/DECOMPOSITION.md`
- `docs/features/*`
- the plugin's own `docs/`

The research memos at the repository root (`TIMESCALEDB-RETENTION.md` and the
three aggregation memos) are inputs. Where this spec differs from them, this
spec wins. [§10](#10-decisions-and-rejected-alternatives) records why.

## 2. Current state

- `usage_records` is a hypertable on `window_end` alone, with the default 7-day
  chunk interval (`migrations/0001_init.sql`).
- `apply_post_migration_setup` registers a single `add_retention_policy`. Its
  horizon is `retention_period_secs`, which defaults to 365 days
  (`src/infra/storage/pool.rs`, `src/config.rs`).
- The plugin never reads `retention` from `types-registry`. It only registers
  its own instance (`src/gear.rs`).
- The only background task is the readiness watcher.

Each chunk holds one week of data for every type. So no chunk can be dropped
for one type without also dropping every other type stored in it.

## 3. The model

TimescaleDB can only cheaply drop data a whole chunk at a time. Per-type
retention therefore needs chunks whose rows all belong to types that expire
together.

The ledger gains a second partitioning dimension: an integer **type key**.

- **Assignment.** A GTS type gets a permanent integer key the first time the
  plugin writes an entry of that type. The key never changes and is never
  reused.
- **Partitioning.** The hypertable partitions on `window_end` × `type_key`.
- **Sweep.** A background **retention sweep** inside the plugin works through
  the chunks. For each one it:
  1. reads the chunk's time range and key range;
  2. resolves the **current** retention of every type in that key range from
     `types-registry`;
  3. drops the chunk once all of those types have expired.

Consequences:

- **Amendments apply to stored data.** An amended retention applies to every
  stored entry, in both directions. Raising it keeps existing chunks longer.
  Lowering it drops them sooner.
- **No declared attribute is stored.** ADR-0015 statement 6 forbids storing a
  declared attribute on an entry. The type key is not one: it is an internal
  identifier for `gts_type_id`.
- **Constraint meaning is unchanged.** The dedup identity, the primary key and
  the invalidation pairing still mean what they did. A type's key never changes,
  so adding it to a unique constraint never separates two rows the constraint
  previously treated as the same.

### 3.1 Slice width

`type_key_slice_width` sets how many consecutive type keys share one slice of
the key dimension.

- **Width 1 (default).** Each chunk holds one type. A chunk is dropped exactly
  when its type's retention has passed the end of its time range.
- **Width N.** Up to N types share a chunk. The chunk is held until the longest
  retention among them has passed. `DESIGN.md` §3.1 permits that over-retention.

Changing the width affects only chunks created afterwards. Existing chunks keep
their slices (verified on 2.29.2). The sweep reads each chunk's actual key range
from the catalog, so chunks of different widths coexist correctly.

Chunk count is the cost of this model. It is roughly *(types ÷ width)* ×
*(weeks retained)*. Width is the setting that bounds it without a data
migration.

## 4. Schema

`migrations/0001_init.sql` is edited in place. The gear is unreleased, and the
file's header already records replacing the schema outright for that reason.
Any existing development database must be recreated, because sqlx rejects a
changed checksum.

Keep the file's formatting conventions: `CREATE TABLE IF NOT EXISTS
usage_records (`, and column lines indented by four spaces.
`src/infra/storage/migration_probe.rs` parses both.

### 4.1 `usage_type_key`

```sql
CREATE TABLE IF NOT EXISTS usage_type_key (
    gts_type_id text NOT NULL PRIMARY KEY,
    type_key    int  GENERATED ALWAYS AS IDENTITY UNIQUE
);
```

This is a plain table, not a hypertable. It maps types to keys and nothing
more. It is not a type catalog:

- it stores no declared attribute (no fold, unit, schema or retention);
- only the plugin reads it.

Gaps in the identity sequence are harmless.

### 4.2 `usage_records`

- New column `type_key int NOT NULL`, placed after `gts_type_id`.
- `PRIMARY KEY (id, window_end, type_key)`.
- `usage_records_dedup_uniq UNIQUE (tenant_id, gts_type_id, idempotency_key,
  window_start, window_end, type_key)`.
- `usage_records_one_invalidation_uniq ON usage_records (invalidates,
  window_end, type_key) WHERE invalidates IS NOT NULL`. An invalidation has
  the same type as its target, and so the same key.

TimescaleDB requires every unique constraint on a hypertable to contain every
partitioning column. If `type_key` is missing from any of the three,
`add_dimension` fails with `cannot create a unique index without the column
"type_key"` (verified on 2.29.2).

The comment on each constraint must say why `type_key` is present and why it
does not change the constraint's meaning.

### 4.3 Hypertable

```sql
SELECT create_hypertable('usage_records', by_range('window_end'), if_not_exists => TRUE);
SELECT add_dimension('usage_records', by_range('type_key', 1), if_not_exists => TRUE);
```

The migration fixes the dimensions. The intervals come from configuration and
are applied at startup ([§7](#7-configuration-and-startup)).

The existing secondary indexes stay unchanged.

## 5. Write path

`PgRecordStore` resolves a record's type key before it inserts the record.

- **Cache.** An in-process `TypeKeyCache` maps `gts_type_id` to `type_key`.
  Entries never expire, because a key never changes. It holds no negative
  entries, and only the write path uses it.
- **Lookup on a miss.** It runs `INSERT INTO usage_type_key (gts_type_id) VALUES
  ($1) ON CONFLICT (gts_type_id) DO NOTHING`, then `SELECT type_key FROM
  usage_type_key WHERE gts_type_id = $1`. If two replicas race on a new type,
  both read the same key.
- **Timing.** The lookup runs in autocommit on the write's own connection,
  **before** the insert transaction opens. So a rolled-back insert never rolls
  back a key that another writer is already waiting on.
- **Batches.** A batch resolves one key per distinct-key representative before
  the insert. Cache hits cost no round trip.
- **Failures.** A failed lookup is mapped through the existing backend error
  classification.

`type_key` joins `INSERT_COLUMNS`, `RECORD_COLUMNS`,
`INSERT_COLUMN_ARRAY_TYPES` (as `int`), `InsertColumns`, `UsageRecordRow` and
`DEDUP_CONFLICT_TARGET`. It goes directly after `gts_type_id`. `metadata` stays
last.

The write path makes no `types-registry` call.

## 6. Read paths

### 6.1 Raw list and aggregate

Both paths select one type through `push_meter_and_range_clauses`. A predicate
on `gts_type_id` alone does **not** exclude other keys' chunks (verified on
2.29.2).

The shared builder therefore adds a second clause, which reuses the meter's
bind:

```sql
r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = $1)
```

- **Chunk exclusion works.** TimescaleDB evaluates the subquery once, as an
  InitPlan, and excludes other keys' chunks at runtime. This holds under both
  generic and custom plans (verified on 2.29.2).
- **No read-side lookup.** Reads need no Rust-side lookup and no cache. The
  existing tests that a statement which cannot be rendered never reaches the
  pool still hold.
- **Unknown types.** A type with no key yields `NULL`. `NULL` matches no row,
  so every chunk is excluded. The result is the ordinary empty one, and an
  ungrouped aggregate still returns its single bucket.
- **Withdrawal subquery.** The aggregate's withdrawal subquery adds
  `w.type_key = r.type_key`, so it stays within the outer row's type.

### 6.2 Get by id

`get_usage_record(id, scope)` carries no type, and stays unchanged.

## 7. Configuration and startup

### 7.1 `TimescaleDbPluginConfig`

| Key | Default | Validation |
| --- | --- | --- |
| `retention_period_secs` | **removed** | — |
| `chunk_time_interval_secs` | 604 800 (7 days) | `> 0` and ≤ 100 years |
| `type_key_slice_width` | 1 | `>= 1` and ≤ `i32::MAX` |
| `retention_sweep_interval_secs` | 3 600 (1 hour) | `> 0` and ≤ 100 years |

The struct is `deny_unknown_fields`, so a config that still carries
`retention_period_secs` fails validation. That is acceptable while the gear is
unreleased.

Other config changes:

- The README must drop `retention_period_secs`.
- Rename `MAX_RETENTION_SECS` to `MAX_INTERVAL_SECS`.
- Fix the stale "dedup-cleanup job" comment in `src/config.rs`.

### 7.2 `apply_post_migration_setup`

`apply_post_migration_setup(pool, chunk_time_interval_secs,
type_key_slice_width)` keeps the init advisory lock and the dedicated-connection
pattern. It does three things:

1. `SELECT remove_retention_policy('usage_records', if_exists => TRUE)`. This
   removes any policy left by an earlier build, so no table-wide drop can run
   alongside the sweep.
2. `SELECT set_chunk_time_interval('usage_records', make_interval(secs =>
   $1::double precision), dimension_name => 'window_end')`. The dimension name
   is required: with two dimensions, TimescaleDB refuses the call without it
   (`hypertable "usage_records" has multiple time dimensions`, verified on
   2.29.2).
3. `SELECT set_chunk_time_interval('usage_records', $1::bigint, dimension_name
   => 'type_key')`.

Both intervals apply to new chunks only (verified on 2.29.2).

## 8. Retention sweep

### 8.1 Components

| Unit | Location | Responsibility |
| --- | --- | --- |
| `RetentionSource`, `RetentionError` | `src/domain/ports.rs` | `async fn retention(&self, gts_type_id: &str) -> Result<Duration, RetentionError>` |
| `drop_decision`, `Decision`, `KeepReason` | `src/domain/retention.rs` | Pure: `(time_end, retentions, now)` → `Drop` or `Keep(reason)` |
| `TypesRegistryRetentionSource` | `src/infra/registry_retention.rs` | Resolves `TypesRegistryClient` from `ClientHub` per call, calls `get_type_schema`, reads `effective_traits()["retention"]`, parses it with `toolkit_utils::iso8601_duration::Iso8601Duration` |
| `PgRetentionSweeper`, `list_chunks` | `src/infra/storage/retention_sweep.rs` | One sweep: lock, list, resolve, decide, drop, report |

`RetentionError` has four variants:

- `Unavailable(detail)`: the client is missing, or the call failed with anything
  other than not-found. Not-found is classified through `TypesRegistryError::from`.
- `NotFound`.
- `MissingTrait`.
- `InvalidTrait(detail)`: the value is not a string, is unparseable, or is zero.

`Iso8601Duration` rejects years and months, so they land in `InvalidTrait`. That
matches the fixed-length requirement.

Dependencies: the plugin crate gains `toolkit-utils = { workspace = true }`, and
a `types-registry-sdk` dev-dependency with `test-util`.

### 8.2 One sweep

1. **Lock.** Acquire a pool connection, **detach** it, and take
   `pg_try_advisory_lock` on it. The key differs from the init lock's.
   - If the lock is held, record a skipped sweep and return.
   - The lock is session-level and lives exactly as long as the detached
     connection. Closing that connection releases it on every path, including
     an abandoned sweep, and a locked connection never returns to the pool.
   - Within one session the lock is re-entrant. Only another session sees it as
     held.
2. **List chunks** from the TimescaleDB catalog:

   ```sql
   SELECT ch.relid::text AS chunk,
          _timescaledb_functions.to_timestamp(
              max(ds.range_end) FILTER (WHERE d.column_name = 'window_end')) AS time_end,
          max(ds.range_start) FILTER (WHERE d.column_name = 'type_key') AS key_start,
          max(ds.range_end)   FILTER (WHERE d.column_name = 'type_key') AS key_end
   FROM _timescaledb_catalog.chunk ch
   JOIN _timescaledb_catalog.hypertable h       ON h.id = ch.hypertable_id
   JOIN _timescaledb_catalog.dimension_slice ds ON ds.chunk_id = ch.id
   JOIN _timescaledb_catalog.dimension d        ON d.id = ds.dimension_id
   WHERE h.table_name = 'usage_records'
   GROUP BY ch.relid
   ```

   - The query collapses each chunk to one row **before** any arithmetic
     (`TIMESCALEDB-RETENTION.md` §8.1).
   - It reads internal catalog tables whose shape changed between 2.17 and 2.29.
     A schema test pins it ([§11.2](#112-integration-docker)).
   - A chunk missing either range is logged and kept.
3. **Load keys.** Load `usage_type_key` into an ordered map, so each chunk's key
   range `[key_start, key_end)` becomes a range lookup.
4. **Resolve retentions.** Resolve each distinct type's retention through
   `RetentionSource`. Results are cached **for this sweep only**, because
   retention is mutable.
5. **Decide.** Run `drop_decision` on each chunk:
   - If no type maps to the key range, the result is `Keep(NoType)`.
   - If any type failed to resolve, the result is `Keep` with that error's kind.
     The sweep **never** drops a chunk without a definite retention for every
     type the chunk may hold.
   - Otherwise the deadline is `time_end + max(retentions)`. The result is `Drop`
     when `deadline < now`, and `Keep(NotExpired)` otherwise.
   - A retention too long to add to an instant is `Keep(NotExpired)`.
6. **Drop.** Drop each `Drop` chunk with `SELECT
   _timescaledb_functions.drop_chunk($1::regclass)`, one statement per chunk. A
   failed drop is logged and counted, and the sweep continues.
7. **Report.** Set the chunk gauge. Close the lock connection. Return a
   `SweepReport { skipped_locked, chunks_seen, dropped, kept_unresolved,
   drop_failures }`.

`time_end` is an upper bound on every `window_end` in the chunk. So the deadline
is never earlier than any entry's own retention deadline, and a purge never
frees a dedup identity before its horizon.

### 8.3 Lifecycle

The plugin gear declares `capabilities = [stateful]` and implements
`RunnableCapability`, following `gears/file-storage/file-storage/src/gear.rs`:

- `init` builds the `PgRetentionSweeper` and stores it for `start`.
- `start(cancel)` spawns a loop. It sweeps immediately, then once every
  `retention_sweep_interval_secs`, until `cancel` fires.
  - Cancellation is observed between sweeps only.
  - `start` before `init`, or a second `start`, is refused.
- `stop(deadline)` cancels the loop and waits for it, up to the deadline.

A sweep error never ends the loop. It is logged and counted, and the next tick
retries.

### 8.4 Races

- **A write into a dropped range.** The insert recreates the chunk (verified on
  2.29.2), and a later sweep collects it once it expires again. Ordinary writes
  cannot reach such a range: the live path bounds `window_end` to 48 hours in
  the past, and `fr-backfill` forbids a backfill window wider than a type's
  retention.
- **A dedup read racing a drop.** This is covered by the existing `dedup_stale`
  → `Transient` handling in `record_store.rs`.
- **Several replicas.** The advisory lock admits one sweeper at a time. Drops
  are idempotent across sweeps.

## 9. Out of scope

- **Rollups and continuous aggregates.** These are the next design cycle, and
  they build on the dimensions this spec fixes.
- **The usage feed** and its multi-type chunk fan-out.
- **Compression and recompression policies.**
- **Enforcing the retention floor.** The plugin cannot know which meters a
  charging consumer reads, so the floor stays a deployment-review obligation
  (`fr-billing-retention-floor`).
- **Stale documentation.** This work does not update `docs/DECOMPOSITION.md`,
  `docs/features/*` or the plugin's own `docs/`.
- **The research memos** at the repository root. They are not edited.

## 10. Decisions and rejected alternatives

| Alternative | Why rejected |
| --- | --- |
| Stamp the retention on each row as a second dimension (`TIMESCALEDB-RETENTION.md`) | The stamp would have to be part of every unique constraint. A retry or an invalidation accepted after an amendment would carry a different stamp, escape the dedup constraint, and be stored twice. Fixing that needs a per-(type, day) mapping table with its own cleanup. Stamping also stops amendments from reaching stored entries, so raising a retention would free dedup identities early (`DESIGN.md` §3.1, ADR-0008). |
| Time-only chunks, held until the longest retention of any type written into them | This is equivalent to an unbounded slice width that can never be narrowed. A single long-retention type holds every chunk. |
| One table per type | It needs DDL on the write path, and the object count grows with the number of types. |
| Row-level `DELETE` per type | It causes bloat and vacuum load on an append-heavy hypertable. |
| A TimescaleDB `add_job` in PL/pgSQL | Retention lives in `types-registry`, which the database cannot reach. |
| Fall back to the last known retention when the registry fails | It adds state, and can drop chunks based on a stale value. Keeping the chunk loses nothing. |
| A Rust-side type-key lookup on reads | It would put a database round trip ahead of statement rendering. The in-SQL subquery excludes the same chunks without one. |

These points were settled in review:

- **Fail-safe:** when a retention is unresolved, the chunk is kept, counted and
  logged.
- **Defaults:** a 1-hour sweep, 7-day chunks, and slice width 1.
- **Migration:** `0001_init.sql` is edited in place.

## 11. Testing

### 11.1 Unit (no Docker)

- **`drop_decision`:**
  - expired chunk;
  - not expired;
  - boundary (`deadline == now` keeps);
  - each `Keep` reason;
  - the longest retention across a shared slice;
  - an unresolved type keeping a chunk whose other type has expired;
  - overflow.
- **`retention_from_traits`:**
  - `P125D` parses, and hours are accepted;
  - `P1Y`, `P1M` and `PT0S` are rejected;
  - a non-string value is rejected, and a missing trait is reported.
- **`TypesRegistryRetentionSource`** over `MockTypesRegistryClient`:
  - a registered type resolves;
  - an unregistered one maps to `NotFound`;
  - a malformed id and a missing client map to `Unavailable`.
- **Config:** the new defaults, validation bounds, and rejection of
  `retention_period_secs`.
- **SQL pins:**
  - insert column order, with `type_key` after `gts_type_id` and `metadata` last;
  - `type_key` in the conflict target;
  - the range builder's four clauses;
  - the withdrawal subquery correlated on `type_key`.
- **`TypeKeyCache`:** a remembered key is served, and an unknown one is not.
- **Metrics:** every new instrument is exported and named by convention.
- **Lifecycle:** `start` before `init` is refused, and `stop` without `start`
  succeeds.

### 11.2 Integration (Docker)

**Schema**

- There are two dimensions, `window_end` then `type_key`.
- The primary key, the dedup key and the invalidation index each contain
  `type_key`.
- `usage_type_key` exists.
- No `policy_retention` job exists after setup.
- The configured intervals appear in `timescaledb_information.dimensions`.
- `list_chunks` returns one row per chunk, with width-1 key ranges. This is the
  pin that fails on a TimescaleDB upgrade that reshapes the catalog.

**Type keys**

- Concurrent first writes of one new type, from stores with cold caches, yield
  one key.
- Distinct types get distinct keys, through both the single and batch insert.

**Reads**

- The existing query suites pass unchanged.
- `EXPLAIN ANALYZE` of a single-type read executes only that type's chunk.

**Sweep** (over a stub `RetentionSource`)

- With mixed retentions, only the expired type's chunks drop.
- An amendment applies both ways: a raised retention keeps, and a lowered one
  drops.
- An unresolvable type keeps its chunks, while other types' chunks still drop.
- At slice width 4, a shared chunk is held until the longest retention passes.
- An invalidation and its target drop together.
- A dropped range accepts a new write, and the next sweep collects it.
- While another session holds the lock, the sweep skips.

**Existing suites**

- `cleanup_integration_pg.rs` loses its policy tests. Its lock tests move to the
  new setup.
- `tests/common/mod.rs` loses `NO_DROP_RETENTION_SECS`, `REAL_RETENTION_SECS` and
  `bring_up_real_retention`. Nothing drops data unless a test runs a sweep.
- `contract_conformance_pg.rs` keeps passing.

## 12. Observability

New instruments in `src/infra/metrics.rs`, also listed in
`declared_instrument_names`:

| Name | Kind | Labels |
| --- | --- | --- |
| `uc_timescaledb_retention_sweeps_total` | counter | `outcome` = `completed` \| `skipped_locked` \| `failed` |
| `uc_timescaledb_retention_sweep_duration_seconds` | histogram | — |
| `uc_timescaledb_retention_chunks_dropped_total` | counter | — |
| `uc_timescaledb_retention_chunks_kept_unresolved_total` | counter | `reason` = `unavailable` \| `not_found` \| `missing_trait` \| `invalid_trait` \| `no_type` |
| `uc_timescaledb_retention_drop_failures_total` | counter | — |
| `uc_timescaledb_chunks` | gauge, set by each completed sweep | — |

## 13. Documentation

The plugin `README.md` gains a Retention section. It is the deployment-guide
statement that `DESIGN.md` §3.10 item 5 requires, and it states:

- **What is enforced.** The plugin enforces each type's **current** declared
  `retention`, measured from `window_end`.
- **Amendments.** An amendment applies to stored entries in both directions,
  within one sweep interval.
- **Over-retention.** A chunk is held up to one chunk interval past its
  retention, and until the longest retention in a shared slice.
- **Unresolved retention.** Data is kept.
  `uc_timescaledb_retention_chunks_kept_unresolved_total` is the signal to act
  on.
- **Chunk count.** It is roughly *(types ÷ `type_key_slice_width`)* × *(weeks
  retained)*. Keep `uc_timescaledb_chunks` below 10 000. Raise the slice width
  or the chunk interval before reaching that.
- **The floor.** Every charging-consumer meter's retention must meet the floor.
  The plugin does not check it.

The README configuration table and the storage-semantics bullets are updated to
match §4 and §7.1.

No governing document changes. `DIVERGENCES.md` entry 21 is updated, though,
because `type_key` narrows the invalidation index's at-most-one guarantee and
the aggregate fold's withdrawal exclusion to withdrawals that share a target's
type, not only its `window_end`.

## 14. Risks

| Risk | Mitigation |
| --- | --- |
| Internal catalog tables change on a TimescaleDB upgrade | The §11.2 pin test fails, and the query is isolated in `list_chunks`. |
| `_timescaledb_functions.drop_chunk` is an internal function | The same pin coverage applies. `DROP TABLE <chunk>` is an equivalent fallback, also verified on 2.29.2. |
| Chunk count grows with the number of types | Slice width and chunk interval can be tuned without a migration. The gauge and the documented ceiling cover the rest. |
| A registry outage stops all drops | Storage grows, but nothing is lost. The unresolved counter surfaces it. |
