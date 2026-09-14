# TimescaleDB plugin — per-type retention

**Date**: 2026-09-14
**Status**: Draft, pending review
**Scope**: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`

## 1. Why

`docs/DESIGN.md` declares retention per GTS type, and the storage plugin enforces
it:

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

The plugin enforces one retention for the whole table. This spec replaces it
with per-type retention.

Rollups are a separate design cycle. See [§9](#9-out-of-scope).

### Documents that are current, and documents that are stale

`docs/DESIGN.md`, `docs/PRD.md`, the ADR set and `docs/schemas/*` are current.
`docs/DECOMPOSITION.md`, `docs/features/*` and the plugin's own `docs/` are
stale. Do not implement from them.

The research memos at the repository root (`TIMESCALEDB-RETENTION.md` and the
three aggregation memos) are inputs. Where this spec differs from them, this
spec wins. [§10](#10-decisions-and-rejected-alternatives) records why.

## 2. Current state

- `usage_records` is a hypertable on `window_end` alone, with the default 7-day
  chunk interval (`migrations/0001_init.sql`).
- `apply_post_migration_setup` registers one `add_retention_policy` from
  `retention_period_secs`, default 365 days (`src/infra/storage/pool.rs`,
  `src/config.rs`).
- The plugin never reads `retention` from `types-registry`. It only registers
  its own instance (`src/gear.rs`).
- The plugin has no background task other than the readiness watcher.

A chunk holds one week of every type. No chunk can be dropped for one type
without dropping every other type in it.

## 3. The model

TimescaleDB drops data cheaply only as whole chunks. Per-type retention
therefore needs chunks whose rows all belong to types that expire together.

The ledger gains a second partitioning dimension: an integer **type key**.

- Every GTS type gets a permanent integer key the first time the plugin writes
  an entry of that type. The key never changes and is never reused.
- The hypertable partitions on `window_end` × `type_key`.
- A background **retention sweep** inside the plugin reads each chunk's time
  range and key range. It resolves the **current** retention of every type in
  that key range from `types-registry`, and drops the chunk once all of them
  have expired.

Consequences:

- An amended retention applies to all stored entries, in both directions. A
  raised retention keeps existing chunks longer. A lowered one drops them
  sooner.
- No declared attribute is stored on an entry (ADR-0015 statement 6). The type
  key is an internal identifier for `gts_type_id`, not a declared attribute.
- The dedup identity, the primary key and the invalidation pairing are
  unchanged in meaning. A type's key is constant, so adding it to a unique
  constraint never separates two rows that the constraint joined before.

### 3.1 Slice width

`type_key_slice_width` sets how many consecutive type keys share one slice of
the key dimension.

- **Width 1 (default).** One type per chunk. A chunk is dropped exactly when its
  type's retention has passed its time range end.
- **Width N.** Up to N types share a chunk. The chunk is held until the longest
  retention among them has passed. That over-retention is permitted
  (`DESIGN.md` §3.1).

Changing the width affects only chunks created afterwards. Existing chunks keep
their slices. The sweep reads each chunk's actual key range from the catalog, so
chunks of mixed widths coexist correctly.

Chunk count is the cost of this model: roughly *(types ÷ width)* × *(weeks
retained)*. Width is the knob that bounds it without a data migration.

## 4. Schema

`migrations/0001_init.sql` is edited in place. The gear is unreleased, and the
file's header already records replacing the schema outright for that reason.
An existing development database must be recreated, because sqlx rejects a
changed checksum.

Keep the file's formatting conventions: `CREATE TABLE IF NOT EXISTS
usage_records (` and four-space-indented column lines.
`src/infra/storage/migration_probe.rs` parses both.

### 4.1 `usage_type_key`

```sql
CREATE TABLE IF NOT EXISTS usage_type_key (
    gts_type_id text NOT NULL PRIMARY KEY,
    type_key    int  GENERATED ALWAYS AS IDENTITY UNIQUE
);
```

A plain table, not a hypertable. It maps types to keys only. It stores no
declared attribute and is not a type catalog: it holds no fold, unit, schema or
retention, and nothing but the plugin reads it.

Identity gaps are harmless. A key is used only after its row commits.

### 4.2 `usage_records`

- New column `type_key int NOT NULL`, placed after `gts_type_id`.
- `PRIMARY KEY (id, window_end, type_key)`.
- `usage_records_dedup_uniq UNIQUE (tenant_id, gts_type_id, idempotency_key,
  window_start, window_end, type_key)`.
- `usage_records_one_invalidation_uniq ON usage_records (invalidates,
  window_end, type_key) WHERE invalidates IS NOT NULL`. An invalidation has
  the same type as its target, so it has the same key.

TimescaleDB requires every unique constraint on a hypertable to contain every
partitioning column. Without `type_key` in all three, `add_dimension` fails with
`cannot create a unique index without the column "type_key"` (verified on
2.29.2).

The comments on these constraints must say why `type_key` is present and why it
does not change their meaning.

### 4.3 Hypertable

```sql
SELECT create_hypertable('usage_records', by_range('window_end'), if_not_exists => TRUE);
SELECT add_dimension('usage_records', by_range('type_key', 1), if_not_exists => TRUE);
```

The migration fixes the dimensions. The intervals come from configuration and
are applied at startup ([§7](#7-configuration-and-startup)).

The existing secondary indexes stay unchanged.

## 5. Write path

`PgRecordStore` resolves a record's type key before it inserts.

- An in-process `TypeKeyCache` maps `gts_type_id` to `type_key`. An entry never
  expires, because a key never changes. It holds no negative entries.
- On a miss, it runs `INSERT INTO usage_type_key (gts_type_id) VALUES ($1) ON
  CONFLICT (gts_type_id) DO NOTHING`, then `SELECT type_key FROM usage_type_key
  WHERE gts_type_id = $1`. Two replicas racing on a new type both read the same
  key.
- The lookup runs on the pool in autocommit, **before** the insert transaction
  opens. A rolled-back insert then leaves only an unused mapping, which is
  harmless.
- A batch resolves every distinct type in it before the insert.
- A lookup failure maps to the existing backend error classification
  (`Transient` for connection-class errors).

`type_key` joins `INSERT_COLUMNS`, `INSERT_COLUMN_ARRAY_TYPES` (`int4`),
`InsertColumns` and `DEDUP_CONFLICT_TARGET`. It goes before `metadata`, which
must stay last.

The write path makes no `types-registry` call.

## 6. Read paths

### 6.1 Raw list and aggregate

Both paths select one type with `r.gts_type_id = $1`. A predicate on
`gts_type_id` alone does **not** exclude chunks of other keys (verified on
2.29.2). So when the type's key is known, `push_meter_and_range_clauses` also
adds `r.type_key = $k`.

- The key comes from the same `TypeKeyCache`. On a miss, the read performs a
  plain `SELECT`, without inserting.
- If the type has no key, nothing of that type was ever written. The read
  omits the `type_key` predicate and runs unchanged. It returns no rows,
  through the ordinary empty-result shape: an ungrouped aggregate still returns
  its single bucket. No sentinel key is used.
- The aggregate's withdrawal subquery adds `w.type_key = r.type_key`.

### 6.2 Get by id

`get_usage_record(id, scope)` carries no type and stays unchanged.

## 7. Configuration and startup

### 7.1 `TimescaleDbPluginConfig`

| Key | Default | Validation |
| --- | --- | --- |
| `retention_period_secs` | **removed** | — |
| `chunk_time_interval_secs` | 604 800 (7 days) | `> 0` |
| `type_key_slice_width` | 1 | `>= 1` |
| `retention_sweep_interval_secs` | 3 600 (1 hour) | `> 0` |

The struct is `deny_unknown_fields`, so a config still carrying
`retention_period_secs` fails validation. That is acceptable while the gear is
unreleased. The README and every shipped config must drop the key.

Remove `MAX_RETENTION_SECS` and its validation. Fix the stale "dedup-cleanup
job" comment in `src/config.rs`.

### 7.2 `apply_post_migration_setup`

It keeps the init advisory lock and the dedicated-connection pattern, and does
three things:

1. `SELECT remove_retention_policy('usage_records', if_exists => TRUE)`. This
   removes a policy left by an earlier build, so no table-wide drop can run
   alongside the sweep.
2. `SELECT set_chunk_time_interval('usage_records', make_interval(secs =>
   $1))`.
3. `SELECT set_chunk_time_interval('usage_records', $1::bigint,
   dimension_name => 'type_key')`.

Both intervals apply to new chunks only (verified on 2.29.2). The signature
changes from `retention_secs: u64` to the three values it needs.

## 8. Retention sweep

### 8.1 Components

| Unit | Layer | Responsibility |
| --- | --- | --- |
| `RetentionSource` | `domain/ports` | `async fn retention(&self, gts_type_id: &str) -> Result<Duration, RetentionError>` |
| `TypesRegistryRetentionSource` | `infra` | Resolves `TypesRegistryClient` from `ClientHub` per call, calls `get_type_schema`, reads `effective_traits()["retention"]`, parses with `toolkit_utils::iso8601_duration::Iso8601Duration` |
| `ChunkCatalog` | `infra/storage` | Lists chunks with their time range end and key range, maps a key range to type ids, drops one chunk |
| `drop_decision` | `domain` | Pure function: chunk + resolved retentions + `now` → `Drop` or `Keep(reason)` |
| `RetentionSweeper` | `domain` | One sweep: lock, list, resolve, decide, drop, report |

`RetentionError` has four variants:

- `Unavailable`: the registry client is missing, or the call failed with
  anything other than not-found.
- `NotFound`
- `MissingTrait`
- `InvalidTrait(detail)`: not a string, unparseable, or zero.

Years and months are rejected by `Iso8601Duration` and so land in
`InvalidTrait`. That matches the fixed-length requirement.

The plugin crate gains `toolkit-utils = { workspace = true }`. It already
depends on `types-registry-sdk`, and its gear already declares
`deps = [types_registry]`.

### 8.2 One sweep

1. Take a session-level `pg_try_advisory_lock` on a dedicated connection. Its
   key differs from the init lock's key. If the lock is held, record a skipped
   sweep and return.
2. List chunks from the TimescaleDB catalog:

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

   Two points about this query:

   - Each chunk collapses to one row **before** any arithmetic. A chained join
     lets the planner compute on the wrong dimension's range
     (`TIMESCALEDB-RETENTION.md` §8.1).
   - It reads internal catalog tables whose shape changed between 2.17 and 2.29.
     A schema test pins it ([§11.2](#112-integration-docker)).

3. Load every type id in each chunk's key range from `usage_type_key`, using
   `type_key >= key_start AND type_key < key_end`.
4. Resolve each distinct type's retention through `RetentionSource`. Cache
   results **for this sweep only**, because retention is mutable.
5. `drop_decision` for each chunk:
   - If no type maps to the key range, the result is `Keep(NoType)`.
   - If any type failed to resolve, the result is `Keep(<that error kind>)`.
     The chunk is **never** dropped without a definite retention for every
     type it may hold.
   - Otherwise, the deadline is `time_end + max(retentions)`. The result is
     `Drop` when `deadline < now` and `Keep(NotExpired)` otherwise.
6. Drop each `Drop` chunk with `SELECT _timescaledb_functions.drop_chunk($1::regclass)`,
   one statement per chunk. A failed drop is logged and counted, and the sweep
   continues with the next chunk.
7. Release the lock on every path.

`time_end` bounds every `window_end` in the chunk from above. So the deadline
is never earlier than any entry's own retention deadline, and a purge never
frees a dedup identity before its horizon.

### 8.3 Lifecycle

The plugin gear declares `capabilities = [stateful]` and implements
`RunnableCapability`, following `gears/file-storage/file-storage/src/gear.rs`:

- `init` builds the `RetentionSweeper` and stores it for `start`.
- `start(cancel)` spawns a loop. The loop runs a sweep immediately, then one
  every `retention_sweep_interval_secs`, until `cancel` fires.
- `stop(deadline)` cancels the loop and awaits it until the deadline.

A sweep error never ends the loop. It is logged and counted, and the next tick
retries.

### 8.4 Races

- **A write into a chunk that was just dropped.** The insert recreates the
  chunk (verified on 2.29.2), and a later sweep collects it once it expires
  again. Ordinary writes cannot reach such a range, because the live path bounds
  `window_end` to 48 hours back. A backfill window wider than a type's retention
  is already forbidden (`fr-backfill`).
- **A dedup read racing a drop.** This is covered by the existing `dedup_stale`
  → `Transient` handling in `record_store.rs`.
- **Several replicas.** The advisory lock admits one sweeper at a time. Drops
  are idempotent across sweeps.

## 9. Out of scope

- Rollups and continuous aggregates. They are the next design cycle, and they
  build on the dimensions this spec fixes.
- The usage feed and its multi-type chunk fan-out.
- Compression and recompression policies.
- Enforcing the retention floor. The plugin cannot know which meters a charging
  consumer reads. The floor stays a deployment-review obligation
  (`fr-billing-retention-floor`).
- Updating `docs/DECOMPOSITION.md`, `docs/features/*` and the plugin's stale
  `docs/`.
- The research memos at the repository root. They are not edited by this work.

## 10. Decisions and rejected alternatives

| Alternative | Why rejected |
| --- | --- |
| Retention stamped on each row as a second dimension (`TIMESCALEDB-RETENTION.md`) | Every unique constraint would have to contain the stamp. A retry or an invalidation accepted after an amendment carries a different stamp, escapes the dedup constraint, and is stored twice. Fixing it needs a per-(type, day) mapping table with its own cleanup. It also stops amendments reaching stored entries, which frees dedup identities early when retention is raised (`DESIGN.md` §3.1, ADR-0008). |
| Time-only chunks, held to the longest retention of any type written into them | Equivalent to an unbounded slice width that can never be narrowed. One long-retention type holds every chunk. |
| One table per type | DDL on the write path, and an object count that grows with types. |
| Row-level `DELETE` per type | Bloat and vacuum load on an append-heavy hypertable. |
| TimescaleDB `add_job` in PL/pgSQL | Retention lives in `types-registry`, which the database cannot reach. |
| Fall back to the last known retention when the registry fails | Adds state and can drop on a stale value. Keeping the chunk loses nothing. |

Fixed by review:

- **Fail-safe:** a chunk is kept, with a count and a warning, whenever retention
  is unresolved.
- **Defaults:** 1-hour sweep, 7-day chunks, slice width 1.
- **Migration:** schema changes go into `0001_init.sql` in place.

## 11. Testing

### 11.1 Unit (no Docker)

- **`drop_decision`:** expired, not expired, the boundary (`deadline == now`
  keeps), each `Keep` reason, and the maximum across a shared slice.
- **`TypesRegistryRetentionSource` over a stub client:**
  - `P125D` parses.
  - Hours are accepted.
  - `P1Y`, `P1M` and `PT0S` are rejected.
  - A non-string trait is rejected.
  - A missing trait is rejected.
  - A not-found error maps to `NotFound`; any other error maps to `Unavailable`.
- **Config:** new defaults, validation bounds, and rejection of
  `retention_period_secs`.
- **SQL pins:**
  - Insert column order with `metadata` last.
  - `type_key` in the conflict target.
  - The range clause carries `r.type_key` when a key is known and omits it
    otherwise.
  - The withdrawal subquery correlates on `type_key`.
- **`TypeKeyCache`:** a hit skips the database, and no negative entry is ever
  cached.

### 11.2 Integration (Docker)

**Schema**

- Two dimensions exist: `window_end` and `type_key`.
- The primary key, the dedup key and the invalidation index each contain
  `type_key`.
- No `policy_retention` job exists after setup.
- The configured intervals appear in `timescaledb_information.dimensions`.
- The catalog query of §8.2 returns one row per chunk with non-null fields.
  This is the pin that fails on a TimescaleDB upgrade that reshapes the catalog.

**Type keys**

- Concurrent first writes of one new type from several tasks yield one key.
- Keys differ across types.

**Reads**

- The existing query suites pass unchanged.
- `EXPLAIN` on a single-type aggregate touches only that key's chunks.

**Sweep** (over a stub `RetentionSource`, with the sweeper called directly
rather than through the loop)

- Mixed retentions: only the expired type's chunks drop.
- Raised retention: a chunk that would have expired is kept.
- Lowered retention: it drops.
- A registry error, not-found, or missing trait for one type keeps its chunks
  and still drops other types' chunks.
- Slice width 4: a shared chunk is held until the longest retention.
- An invalidation and its target drop together.
- A dropped range accepts a new write, and the next sweep handles it.
- A held advisory lock makes the sweep skip.

**Existing suites**

- `cleanup_integration_pg.rs` loses its policy tests. Its lock tests move to the
  new setup.
- `tests/common/mod.rs` removes `NO_DROP_RETENTION_SECS`,
  `REAL_RETENTION_SECS` and `bring_up_real_retention`. Nothing drops data
  unless a test runs a sweep.
- Any raw-SQL fixture insert must supply `type_key`, or insert through
  `PgRecordStore`.
- `contract_conformance_pg.rs` must keep passing.

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
| `uc_timescaledb_chunks` | observable gauge | — |

## 13. Documentation

The plugin `README.md` gains a retention section. It is the deployment-guide
statement `DESIGN.md` §3.10 item 5 requires, and it states:

- The plugin enforces each type's **current** declared `retention`, measured
  from `window_end`.
- An amendment applies to stored entries in both directions, within one sweep
  interval.
- A chunk is held up to one chunk interval past its retention, and to the
  longest retention in a shared slice.
- Unresolved retention keeps data. `uc_timescaledb_retention_chunks_kept_unresolved_total`
  is the signal to act on.
- Chunk count is roughly *(types ÷ `type_key_slice_width`)* × *(weeks
  retained)*. The section recommends a ceiling of 10 000 chunks on
  `uc_timescaledb_chunks`, and says to raise the slice width or the chunk
  interval before reaching it.
- Every charging-consumer meter's retention must meet the floor. The plugin
  does not check it.

The README configuration table is updated for the §7.1 keys, including the
existing omission of `statement_timeout_secs`.

No governing document changes, and no `DIVERGENCES.md` entry is needed. The
design implements `DESIGN.md` as written.

## 14. Risks

| Risk | Mitigation |
| --- | --- |
| Internal catalog tables change on a TimescaleDB upgrade | The §11.2 pin test fails. The query is isolated in `ChunkCatalog`. |
| `_timescaledb_functions.drop_chunk` is an internal function | The same pin coverage applies. `DROP TABLE <chunk>` is an equivalent fallback, also verified on 2.29.2. |
| Chunk count grows with type count | Slice width and chunk interval can be tuned without migration. The gauge and a documented ceiling cover the rest. |
| A registry outage stops all drops | Storage grows, and nothing is lost. The unresolved counter surfaces it. |
| A single-type read without a known key scans every key in range | This happens only for a type never written, which returns no rows. |
