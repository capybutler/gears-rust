# TimescaleDB plugin — SUM/COUNT rollups

**Date**: 2026-09-14
**Status**: Approved in review, pending spec review
**Scope**: `gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin`

## 1. Why

`nfr-query-latency` requires p95 ≤ 500 ms for a 30-day single-tenant
aggregation, at ≥ 100 concurrent queries while ingesting ≥ 10 000 entries/s.
Every aggregate is served today by a live scan of the ledger
(`record_store.rs`, `build_aggregate_sql`). At the declared volume a 30-day
scan cannot meet that budget.

The governing documents make materialisation legitimate and state what it owes:

- The aggregate path is a derived view, and a plugin may serve it from a
  materialised representation (PRD §5.5, ADR-0011).
- Selecting on `window_end` partitions the entries, so a plugin can serve a
  range from a rollup keyed on that column (ADR-0014).
- A materialised aggregate must recompute over a withdrawn range, rather than
  absorb the withdrawal as a further term (DESIGN §3.1 "Recomputation", PRD
  `fr-record-invalidation`).
- The plugin must publish acceptance → aggregate visibility, and separately
  how an accepted invalidation reaches the aggregate (DESIGN §3.10 item 3,
  `nfr-aggregate-freshness`).

This spec adds an hourly rollup for the `SUM` and `COUNT` folds. It needs no SPI,
SDK or gateway change.

### Documents that are current, and documents that are stale

These documents are current: `docs/DESIGN.md`, `docs/PRD.md`, the ADR set, and
`docs/schemas/*`. These are stale and must not be used as a basis:
`docs/DECOMPOSITION.md`, `docs/features/*`, and the plugin's own `docs/`.

The research memos at the repository root (`INVESTIGATION-PRE-AGGREGATION.md`,
`AGGREGATION-UNDER-END-ASSIGNMENT.md`,
`END-ASSIGNMENT-WITH-WINDOW-END-PARTITIONING.md`) are inputs. Where this spec
differs from them, this spec wins, and [§11](#11-decisions-and-rejected-alternatives)
records why.

## 2. Current state

- The ledger `usage_records` is a hypertable on `window_end` × `type_key`
  (`migrations/0001_init.sql`), delivered by the per-type retention spec
  (`2026-09-14-usage-collector-timescaledb-per-type-retention-design.md`).
- `aggregate` builds one statement per call. It excludes withdrawn pairs with
  `r.invalidates IS NULL AND NOT EXISTS (…)`, and it handles all five folds.
- There is no continuous aggregate, no materialised table, and no refresh job.
- The plugin's background task runs the retention sweep only.

## 3. Scope

In scope:

- a materialised hourly rollup for `SUM` and `COUNT`;
- routing from `aggregate` to the rollup when the query can be answered
  exactly from it;
- keeping the rollup consistent with the ledger's retention;
- publishing the freshness bounds.

Decided in review:

| Decision | Choice |
| --- | --- |
| Folds | `SUM` and `COUNT`. `MAX`, `MIN` and `LATEST` keep the exact scan. |
| Grain | `(tenant_id, gts_type_id, type_key, bucket)` |
| Buckets | 1 hour, one level |
| Mechanism | A native continuous aggregate, maintained by two refresh policies |

## 4. Verified behaviour (TimescaleDB 2.29.2-pg18)

A throwaway probe verified every behaviour this design depends on. The
references below (P1…P21) are used through the rest of the spec.

| # | Behaviour | Result |
| --- | --- | --- |
| P1 | A continuous aggregate over the two-dimension ledger hypertable | Works |
| P16 | An inline `CASE` inside `SUM`, and `CREATE … WITH NO DATA` inside a transaction block | Works. No ledger column is needed, and a normal sqlx migration can create it. |
| P2 | Real-time aggregation: a row above the watermark is visible without a refresh | Works |
| P3 | A write below the watermark stays stale until a refresh, and a refresh over a wide window picks it up from the invalidation log | Works. 1.4 ms; a no-op refresh took 0.8 ms. |
| P4 | A record plus its invalidation nets to `sum 0` and `count 0`, matching the `NOT EXISTS` scan | Works |
| P5, P13 | Two refresh policies on one aggregate, with non-overlapping windows, one of them starting at `NULL` | Both attach, and both ran |
| P6 | `drop_chunk` on the ledger leaves rollup rows behind, and a later refresh does not remove them | Confirmed |
| P11b | A direct `DELETE` on the materialisation hypertable by `type_key` and `bucket` | Works, and no refresh re-creates the rows |
| P17 | `drop_chunk` and that `DELETE` in one transaction | Atomic. A rollback keeps both. |
| P11c | A write into a dropped range recreates the chunk | The next refresh recomputes that hour from the surviving rows only |
| P12 | `type_key = (SELECT …)` on the view | The materialised half uses a `(type_key, bucket)` index |
| P10 | A 28-digit `NUMERIC` sum | Exact |
| P14 | Insert cost with the aggregate present, 500 k-row statement and 5 k single-row statements | 2.38 s against 2.36 s, and equal. The overhead is negligible. |
| P15 | A refresh after 500 k invalidating rows | 212 ms |
| P9 | `refresh_continuous_aggregate` inside a transaction block | Refused. Policies avoid the need. |
| P11a | `refresh_continuous_aggregate(…, force => true)` | Exists. Not used by this design. |
| P19 | The watermark before a refresh, after a refresh over no rows, and after a refresh with rows | `4714-11-24 BC`, unchanged, then the end of the newest materialised bucket (not the refresh window's end) |
| P20 | `timescaledb_information.jobs` for a refresh policy | `hypertable_name` is the **view** (`usage_rollup_1h`), not the materialisation hypertable. `timescaledb_information.continuous_aggregates` names the materialisation hypertable. |
| P21 | A new policy's first run; re-adding a policy with the same offsets; `alter_job(…, scheduled => false)` | Runs within seconds of creation; refused ("already exists"); pauses the job |

**One property shapes the defaults.** A refresh recomputes every group in an
invalidated time range, not only the groups written. So the refresh must never
chase the hours that ingestion is still writing into.
[§5.3](#53-refresh) sets the materialisation lag for that reason.

## 5. Storage and maintenance

### 5.1 Migration

The rollup is added by a new file, `migrations/0002_usage_rollup.sql`.
`0001_init.sql` is not edited, so an existing development database migrates
forward without being recreated.

```sql
CREATE MATERIALIZED VIEW IF NOT EXISTS usage_rollup_1h
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '1 hour', window_end) AS bucket,
       tenant_id,
       gts_type_id,
       type_key,
       SUM(CASE WHEN invalidates IS NULL THEN value ELSE -value END) AS sum_value,
       SUM(CASE WHEN invalidates IS NULL THEN 1 ELSE -1 END)::bigint  AS count_value
FROM usage_records
GROUP BY 1, 2, 3, 4
WITH NO DATA;
```

The file header comment must state:

- why the signed netting is exact ([§5.2](#52-why-signed-netting-is-exact));
- that `type_key` adds no rows, because it is a function of `gts_type_id`, and
  is present so reads and the retention cut can prune by type;
- that the view is plugin-internal and no SPI shape reads it.

### 5.2 Why signed netting is exact

The scan excludes a withdrawn pair. The rollup instead adds `+q` for the record
and `−q` for the invalidation. The two agree because of four invariants, all of
them already enforced:

1. **Same group.** An invalidation copies its target's `window_end`,
   `tenant_id` and type, and therefore its `type_key`. So both entries land in
   one row of the rollup (faithful copy, DESIGN §3.1).
2. **At most one invalidation.** A record carries at most one invalidation
   (`usage_records_one_invalidation_uniq` plus the gateway; `DIVERGENCES.md`
   entry 21).
3. **The target existed.** The gateway accepts an invalidation only for an
   existing record.
4. **The pair is purged together.** The two entries share `window_end` and
   `type_key`, so they share a chunk, and the sweep drops them together.

`COUNT` nets the same way, with `+1` and `−1`.

### 5.3 Refresh

`apply_post_migration_setup` applies two policies, under the init advisory lock
it already holds. On every startup it first deletes the aggregate's existing
policies (`delete_job` over `timescaledb_information.jobs` rows whose
`hypertable_name` is the view, P20), so a configuration change replaces them.
TimescaleDB refuses a second policy with the same offsets (P21), so the delete
is required, not tidy.

| Policy | `start_offset` | `end_offset` | `schedule_interval` | Covers |
| --- | --- | --- | --- | --- |
| live | `rollup_live_window_secs` | `rollup_materialization_lag_secs` | `rollup_refresh_interval_secs` | late live writes, and invalidations of recent periods |
| history | `NULL` | `rollup_live_window_secs` | `rollup_history_refresh_interval_secs` | backfill, and invalidations of old periods, at any age |

- **The materialisation lag.** Buckets newer than the lag are never
  materialised. Real-time aggregation answers them from the ledger, so the
  refresh does not repeatedly recompute hours that ingestion is still writing.
  At the memo's volume estimate, the real-time half reads about 3.6 k rows per
  scope for a 2-hour lag.
- **The live window.** The gateway's live past tolerance defaults to 48 hours.
  A deployment configured wider than `rollup_live_window_secs` stays correct:
  those writes are picked up by the history policy instead, which only makes
  them slower to appear.
- **The history policy.** A refresh over a window with no invalidations is a
  no-op (P3), so an unbounded start costs nothing in the steady state.

### 5.4 Configuration

These keys are added to `TimescaleDbPluginConfig`:

| Key | Default | Validation |
| --- | --- | --- |
| `rollup_materialization_lag_secs` | 7 200 | a multiple of 3 600, ≥ 3 600, and < `rollup_live_window_secs` |
| `rollup_live_window_secs` | 259 200 (3 days) | a multiple of 3 600, and ≤ `MAX_INTERVAL_SECS` |
| `rollup_refresh_interval_secs` | 120 | `> 0` and ≤ `MAX_INTERVAL_SECS` |
| `rollup_history_refresh_interval_secs` | 3 600 | `> 0` and ≤ `MAX_INTERVAL_SECS` |

`chunk_time_interval_secs` gains one rule: it must be a multiple of 3 600. Every
hourly bucket then lies inside exactly one ledger chunk, which the retention cut
relies on ([§7](#7-retention)). The default of 7 days already satisfies it.

## 6. Read path

### 6.1 Eligibility

`aggregate` serves a query from the rollup only when all of these hold.
Otherwise it runs today's `build_aggregate_sql`, unchanged.

1. **Fold.** The fold is `Sum` or `Count`.
2. **Metadata.** `metadata_filter` is empty.
3. **Grouping.** `group_by` is `[]` or `[TenantId]`.
4. **Filter.** Every field the composed `$filter` names is a grain column. The
   composed filter is the caller's filter with the PDP scope folded in. The
   grain columns the translator admits are `tenant_id` alone. A predicate over a
   grain column has the same value on every row of a group, so it commutes with
   the grouping. A predicate on any other field does not:
   - `origin` can differ between a record and its invalidation (a backfill
     withdrawal of a live record), so it can select one of the pair;
   - `entry_type` and `invalidates` select one of the pair by definition;
   - a resource or subject field is not in the grain.
5. **Range.** The range covers at least one whole UTC hour:
   `ceil_hour(from) < floor_hour(to)`.

A fallback records its reason ([§8.2](#82-metrics)).

### 6.2 Hour split

The split is computed in Rust: `a' = ceil_hour(from)` and
`b' = floor_hour(to)`. Hours are aligned to the Unix epoch in UTC, which is the
alignment `time_bucket(INTERVAL '1 hour', …)` uses. `TimeRange` carries
nanoseconds and PostgreSQL stores microseconds, so the ceiling is taken on the
exact instant, and a sub-microsecond lower bound rounds up to the next hour.

### 6.3 Statement

```sql
WITH parts AS (
  SELECT [r.tenant_id::text AS d,] r.sum_value AS s, r.count_value AS c
  FROM usage_rollup_1h r
  WHERE r.gts_type_id = $1
    AND r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = $1)
    AND r.bucket >= $a' AND r.bucket < $b'
    [AND <tenant filter>]
  UNION ALL
  SELECT [r.tenant_id::text AS d,]
         SUM(CASE WHEN r.invalidates IS NULL THEN r.value ELSE -r.value END) AS s,
         SUM(CASE WHEN r.invalidates IS NULL THEN 1 ELSE -1 END) AS c
  FROM usage_records r
  WHERE r.gts_type_id = $1
    AND r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = $1)
    AND ((r.window_end >= $from AND r.window_end < $a')
      OR (r.window_end >= $b'  AND r.window_end < $to))
    [AND <tenant filter>]
  [GROUP BY 1]
)
SELECT [d,] <fold expression>
FROM parts
[GROUP BY 1 HAVING SUM(c) <> 0]
[LIMIT MAX_AGGREGATION_BUCKETS + 1]
```

- **The halves.** The first half reads whole hours from the view, whose
  real-time half covers anything above the watermark. The second half is an
  exact, chunk-excluded scan of the two partial hours, at most one hour each.
  When an edge is empty (`from == a'` or `b' == to`), its disjunct selects
  nothing, and the builder omits it.
- **The tenant filter.** It is `translate_scope` output, rendered once per half
  with its own placeholders. The bare column names it emits are valid against
  both `FROM` clauses. `translate_scope` parenthesises its output, which the
  composition relies on, as the scan does.
- **Identifiers.** Every identifier comes from a closed match. Every value is
  bound.

### 6.4 Result shape

The rollup path must return exactly what the scan returns.

| Case | Scan | Rollup fold expression |
| --- | --- | --- |
| Grouped | one bucket per group with a non-withdrawn entry | `HAVING SUM(c) <> 0` drops a group that netted to zero entries |
| Ungrouped `SUM` | one bucket; `NULL` when nothing is selected | `CASE WHEN COALESCE(SUM(c), 0) = 0 THEN NULL ELSE SUM(s) END::numeric` |
| Ungrouped `COUNT` | one bucket; `0` when nothing is selected | `COALESCE(SUM(c), 0)::numeric` |
| Grouped `SUM` | the group's total | `SUM(s)::numeric` |
| Grouped `COUNT` | the group's count | `SUM(c)::numeric` |

The `HAVING` and the null test read the count, never the sum. So a group whose
genuine measurements sum to zero, or to a negative value, is still returned.

`SUM(c) <> 0` is exactly "at least one non-withdrawn record", because §5.2
makes every group's `c` the number of records minus their invalidations.

### 6.5 Code shape

| Unit | Location | Responsibility |
| --- | --- | --- |
| `rollup_eligible` | `src/infra/storage/query/rollup.rs` | Pure: `(fold, filter, metadata_filter, group_by, range)` → `Ok(HourSplit)` or `Err(FallbackReason)` |
| `HourSplit`, `hour_split` | same | Pure: the `a'` / `b'` split |
| `build_rollup_aggregate_sql` | same | Pure: statement, binds, `dim_count` — the same `AggregateStatement` shape as the scan |
| `filter_fields` | `src/infra/storage/query/translate.rs` | Pure: the field names an `ast::Expr` references |
| `PgRecordStore::aggregate` | `record_store.rs` | Picks the builder, records the path, and shares `aggregate_bucket` |

A test-only switch on `PgRecordStore` forces the scan path, so integration tests
can compare the two paths through the store. It is not reachable through
configuration.

## 7. Retention

**Rule.** The rollup holds no bucket whose ledger rows are gone. Otherwise the
aggregate would state something the entries no longer support (PRD §5.5).
TimescaleDB does not enforce this (P6), so the sweep does.

### 7.1 Changes to `PgRetentionSweeper`

1. **`list_chunks` also returns `time_start`.** It is the same `FILTER` pattern
   as `time_end`. The catalog pin test grows to match.
2. **The sweep resolves the materialisation hypertable** once per sweep:

   ```sql
   SELECT format('%I.%I', materialization_hypertable_schema, materialization_hypertable_name)
   FROM timescaledb_information.continuous_aggregates
   WHERE view_schema = current_schema() AND view_name = 'usage_rollup_1h'
   ```

   If that fails or returns no row, the sweep drops nothing. The failure is
   logged and counted as a failed sweep. A ledger drop without its rollup cut is
   never made, just as a chunk is never dropped without a definite retention.
3. **Each `Drop` decision is one transaction:**

   ```sql
   BEGIN;
   SELECT _timescaledb_functions.drop_chunk($1::regclass);
   DELETE FROM <materialisation hypertable>
    WHERE type_key >= $2 AND type_key < $3
      AND bucket >= $4 AND bucket + INTERVAL '1 hour' <= $5;
   COMMIT;
   ```

   - **Shared slices.** A chunk is dropped only once every type in its key
     range has expired, so cutting the whole key range is correct.
   - **Straddling buckets.** The predicate cuts only buckets that lie wholly
     inside the chunk. With the hour-multiple rule of §5.4 that is every bucket.
     A straddling bucket could only come from a chunk created under an earlier,
     non-conforming interval. It is kept while its neighbour still holds rows,
     so the rollup can over-retain one bucket but never under-count.
   - **Failures.** A failure rolls back both statements. It is logged and
     counted as a drop failure, and the next sweep retries it.
   - **The table name.** The materialisation hypertable name is interpolated
     with `%I` quoting from the catalog, never from input. Every value is bound.

### 7.2 Races

| Race | Outcome |
| --- | --- |
| A refresh is reading the chunk while the sweep drops it | `drop_chunk` waits for the refresh to commit. The `DELETE` runs on a new snapshot and removes what the refresh wrote. |
| The sweep and a refresh deadlock | Postgres aborts one of them. The sweep retries on its next run, and the policy job on its next schedule. |
| A write lands in a dropped range | The insert recreates the chunk. The next refresh recomputes that hour from the surviving rows (P11c), and a later sweep collects both again. |
| A policy refreshes an already-dropped range | Nothing was invalidated there, so nothing is re-materialised (P11b). |

The retention spec's §8.4 race, where a new type is keyed into an
already-expired shared slice, is unchanged.

## 8. Freshness and observability

### 8.1 Published freshness

The plugin README publishes this table, under DESIGN §3.10 item 3:

| Entry's `window_end` | Acceptance → aggregate visibility | Invalidation propagation |
| --- | --- | --- |
| Within `rollup_materialization_lag` of now | Immediate: read from the ledger | Immediate |
| Older, within `rollup_live_window` | ≤ `rollup_refresh_interval` + refresh runtime | Same |
| Older than `rollup_live_window` | ≤ `rollup_history_refresh_interval` + refresh runtime | Same |
| Any query that falls back to the scan | Immediate | Immediate |

- **Defaults.** With them, a late live write or a recent withdrawal is visible
  within 2 minutes plus the refresh runtime. A backfilled period is visible
  within 1 hour plus the refresh runtime.
- **Not measured here.** The README states that p95 conformance under
  `nfr-throughput-profile` must be measured by a load test, and that this
  repository does not run one. The stated numbers are configured bounds, not
  measurements.

### 8.2 Metrics

New instruments in `src/infra/metrics.rs`, also listed in
`declared_instrument_names`:

| Name | Kind | Labels |
| --- | --- | --- |
| `uc_timescaledb_aggregate_path_total` | counter | `path` = `rollup` \| `scan`; `reason` = `none` \| `fold` \| `metadata_filter` \| `group_by` \| `filter_field` \| `sub_hour_range` |
| `uc_timescaledb_rollup_rows_deleted_total` | counter | — |
| `uc_timescaledb_rollup_refresh_age_seconds` | gauge | `policy` = `live` \| `history` |
| `uc_timescaledb_rollup_refresh_job_failing` | gauge, 0 or 1 | `policy` = `live` \| `history` |

- **`path` and `reason`.** `path="rollup"` always carries `reason="none"`.
- **How the gauges are fed.** A monitor tick samples both gauges every 60 seconds
  inside the plugin's existing background task. It reads each policy's
  `last_successful_finish` and `last_run_status` from
  `timescaledb_information.job_stats`. The sweep keeps its own timer. A failed
  sample is logged and leaves the gauges unchanged. The age gauge is not set
  for a policy that has never succeeded.
- **Why not the watermark.** The aggregate's watermark is the end of the newest
  *materialised bucket*, not of the refreshed window, and it stays at the
  minimum timestamp until data exists (P19). A watermark lag therefore alarms on
  an empty or quiet deployment. The time since a policy last succeeded measures
  the refresh itself.
- **Alerting.** A live-policy refresh age above twice
  `rollup_refresh_interval_secs`, or a failing gauge at 1, means the published
  bound is not being kept.

## 9. Documentation

- **The plugin `README.md`** gains an "Aggregate path" section. It states:
  - when the rollup serves a query ([§6.1](#61-eligibility));
  - the freshness table ([§8.1](#81-published-freshness));
  - the four invariants the signed netting depends on ([§5.2](#52-why-signed-netting-is-exact));
  - the retention coupling ([§7](#7-retention)).

  The configuration table gains the four new keys and the hour-multiple rule.
- **`DIVERGENCES.md` entry 21** gains a sentence: the rollup path depends on
  the same at-most-one-invalidation guarantee, so a violation would now skew
  rollup sums as well as the scan.
- **No governing document changes.**

## 10. Testing

### 10.1 Unit (no Docker)

- **`rollup_eligible`:**
  - eligible: a tenant-only filter (`eq`, `in`, and nested `and`/`or` over
    `tenant_id`); `group_by` `[]` and `[TenantId]`;
  - each fallback reason: `Max`, `Min` and `Latest`; a metadata filter; a
    resource, subject or metadata `group_by`; a filter naming `origin`,
    `entry_type`, `invalidates`, `resource_id` or `subject_id`, including one
    inside an `or` with `tenant_id`; a sub-hour range.
- **`hour_split`:**
  - both bounds aligned, both unaligned, and one of each;
  - exactly one hour;
  - a sub-hour range, and one that crosses a boundary without a whole hour;
  - a sub-microsecond bound.
- **`filter_fields`:** nested boolean operators, and `in` lists.
- **SQL pins for `build_rollup_aggregate_sql`:**
  - bind order;
  - the meter and `type_key` subquery in both halves;
  - the tenant filter rendered once per half with distinct placeholders;
  - no predicate names `window_start`;
  - empty edges omitted;
  - `HAVING` only when grouped;
  - the ungrouped `SUM` null-on-zero-count arm, and `COALESCE` for `COUNT`;
  - `LIMIT` only when grouped;
  - `dim_count` matches the select list.
- **Sweep pins:** the drop transaction's statements, and the materialisation
  lookup.
- **Config:** new defaults, bounds, both hour-multiple rules, and lag < live
  window.
- **Metrics:** each new instrument is exported and named by convention, and
  every label value is from the closed set.

### 10.2 Integration (Docker, `timescale/timescaledb:2.29.2-pg18`)

**Harness**

- After setup, the harness waits for both policies' first run (they run on
  creation, P21) and then deletes them, so background workers cannot race
  assertions. Tests refresh through a helper that calls
  `refresh_continuous_aggregate` explicitly. A test about the policies
  themselves re-applies setup.
- Routing is asserted by behaviour rather than by metric: after a refresh, a
  late write below the watermark is invisible on the rollup path until the next
  refresh, and visible at once on a query that falls back (an `origin` filter).
  The path counter's emission is covered by a unit test.

**Schema**

- `usage_rollup_1h` exists, with `materialized_only = false` and the grain
  columns.
- After setup exactly two refresh policies exist, with the configured offsets.
- Running setup a second time replaces the policies rather than adding more.
- Pins for the materialisation lookup and for `list_chunks` with `time_start`.
  These fail on a TimescaleDB upgrade that reshapes the catalog.

**Equivalence** (the load-bearing test)

- A seeded dataset holds several tenants and two types, records, invalidations,
  negative quantities, point events, periods ending exactly on hour boundaries,
  and `origin = backfill` entries.
- After a refresh, the test queries many ranges: aligned, unaligned, sub-hour,
  crossing the watermark, and covering fully withdrawn tenants. It runs each for
  `SUM` and `COUNT`, grouped and ungrouped.
- The rollup path's buckets equal the forced scan path's buckets, value for value
  and in set equality of keys.

**Edge semantics**

- A tenant whose only entries are a withdrawn pair has no grouped bucket.
- Ungrouped `SUM` over that tenant is `NULL`, and `COUNT` is `0`.

**Routing**

- After a refresh, a late write below the watermark does not change an eligible
  query's result until the next refresh, and does change the same query with an
  `origin` filter at once, because that query falls back to the scan.

**Freshness**

- A write inside the materialisation lag is visible immediately.
- A write below the watermark reads stale on the rollup path until a refresh,
  and correct after one.

**Sweep**

- A drop removes that type's rollup rows, and leaves other types' rows.
- At slice width 4, the whole key range's rows are cut.
- With the materialisation lookup failing, no chunk is dropped.
- A write into a dropped range reads correct after a refresh.

**Existing suites**

- They pass unchanged, including `contract_conformance_pg.rs`.
- The SDK contract checks use ranges shorter than an hour, so they exercise the
  scan path only. The equivalence test is what holds the rollup path to the same
  rules.

## 11. Decisions and rejected alternatives

| Alternative | Why rejected |
| --- | --- |
| A rollup table updated in the ingest transaction | It is exact and has no lag, but it adds work to the 75 ms ingestion budget and vacuum load on hot rows. A continuous aggregate costs ingestion nothing (P14). |
| An asynchronous plugin loop from an acceptance watermark | A watermark can skip an entry whose transaction commits late, and the rollup is then silently wrong (`AGGREGATION-UNDER-END-ASSIGNMENT.md` §4.3). |
| A `refresh_queue` of old invalidations (memo §4.4) | The continuous aggregate's own invalidation log records old writes, and the history policy refreshes them (P3, P13). |
| A plugin-side refresh loop | Two native policies on one aggregate are accepted (P13). A refresh cannot run inside a transaction block anyway (P9). |
| Stored generated `signed_value` columns | The inline `CASE` works (P16), and a column would add storage to every ledger row. |
| `MAX`/`MIN` via a records-only aggregate plus a withdrawn-bucket index, and a plugin `LATEST` rollup | Out of scope for this cycle, by decision. |
| Resource, subject or metadata dimensions in the grain | They bring the rollup close to a re-index of the ledger. Those queries fall back. |
| A daily level above the hourly one | 720 rows per 30-day query is already about 0.14 % of the budget. |
| A 1-hour materialisation lag | A refresh recomputes every group in an invalidated range, so it would recompute the hour ingestion is still writing on almost every run. |
| Cutting the rollup through `refresh_continuous_aggregate(…, force => true)` after a drop | It recomputes every type in the chunk's time range. A targeted `DELETE` touches only the dropped types. |
| Rollup rows outliving the ledger | The aggregate would state something the entries no longer support (PRD §5.5). |
| Letting rollup reads apply an `origin` or `entry_type` filter | It can select one entry of a withdrawn pair, and the signed sum then disagrees with the scan. |

## 12. Out of scope

- rollups for `MAX`, `MIN` and `LATEST`;
- dimensions beyond the grain;
- compression of the ledger or the rollup;
- the usage feed;
- load testing against `nfr-throughput-profile`;
- stale documentation: `docs/DECOMPOSITION.md`, `docs/features/*` and the
  plugin's own `docs/`;
- the research memos, which are not edited.

## 13. Risks

| Risk | Mitigation |
| --- | --- |
| The internal catalog, `cagg_watermark` or `drop_chunk` change on an upgrade | The §10.2 pin tests fail. Each query is isolated in one function. |
| Refresh runtime grows with the rows written into old periods | The history policy runs hourly. `uc_timescaledb_rollup_refresh_job_failing` and `uc_timescaledb_rollup_refresh_age_seconds` surface a job that falls behind. |
| A deployment's live past tolerance exceeds `rollup_live_window_secs` | Correctness holds. Those writes appear on the history schedule, and the README says to size the window to the tolerance. |
| The at-most-one-invalidation guarantee is violated | The rollup would skew along with the scan. `DIVERGENCES.md` entry 21 records the dependency. |
| Background workers are disabled on a deployment | If they never ran, the watermark never advances and every query answers correctly through the real-time half, only slowly. If they stop after running, writes below the watermark stay stale indefinitely. The README states that background workers are required, and `uc_timescaledb_rollup_refresh_age_seconds` and `uc_timescaledb_rollup_refresh_job_failing` show both cases. |
